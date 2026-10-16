//! Orchestration: quiesce, snapshot, package, encrypt, upload.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::Context as _;
use jiff::Timestamp;
use serde::Serialize;
use tokio::{sync::Mutex, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use uptime_store::Store;

use crate::{
    BackupSettings,
    archive::{self, Entry},
    s3::{S3Client, utc_stamp},
};

/// A backup failed before the archive reached the bucket.
#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("a backup is already running")]
    Busy,
    #[error("the database snapshot did not finish within {0:?}")]
    SnapshotTimeout(Duration),
    #[error("store: {0}")]
    Store(#[from] uptime_store::StoreError),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Where the archive points for the secrets key needed to restore it.
const SECRETS_KEY: &str = "recovery/secrets-key.age";

/// What a completed run verified.
#[derive(Clone, Debug, Serialize)]
pub struct Verified {
    /// How the snapshot's consistency was obtained.
    pub method: String,
    pub monitors: i64,
    pub status_pages: i64,
    pub check_results: i64,
}

/// The JSON report uploaded next to the archive.
#[derive(Clone, Debug, Serialize)]
pub struct BackupReport {
    pub format: u32,
    pub created_at: String,
    pub engine: String,
    pub database: String,
    pub object_key: String,
    pub archive_sha256: String,
    pub archive_bytes: u64,
    pub verified: Verified,
    pub secrets_key: String,
}

/// A finished local run plus the background upload of its archive.
pub struct BackupRun {
    pub report: BackupReport,
    /// Resolves once the archive and its report are in the bucket.
    pub upload: JoinHandle<anyhow::Result<()>>,
}

/// Runs encrypted backups of one store to one S3-compatible bucket.
pub struct Backup {
    store: Store,
    settings: BackupSettings,
    s3: S3Client,
    busy: Arc<Mutex<()>>,
}

impl Backup {
    pub fn new(store: Store, mut settings: BackupSettings) -> anyhow::Result<Self> {
        settings.validate()?;
        if !settings.prefix.is_empty() && !settings.prefix.ends_with('/') {
            settings.prefix.push('/');
        }
        let s3 = S3Client::new(&settings.s3)?;
        Ok(Self {
            store,
            settings,
            s3,
            busy: Arc::new(Mutex::new(())),
        })
    }

    /// The next UTC occurrence of the configured time, strictly after `now`.
    pub fn next_run_at(&self, now: Timestamp) -> Timestamp {
        next_run_at(self.settings.at, now)
    }

    /// Runs a backup now, then every day at the configured time, until
    /// `shutdown` is cancelled. Each archive uploads in the background.
    pub async fn run(&self, shutdown: CancellationToken) {
        loop {
            let now = Timestamp::now();
            let next = self.next_run_at(now);
            let wait = Duration::from_secs(
                u64::try_from((next.as_second() - now.as_second()).max(0)).unwrap_or(0),
            );
            tracing::info!(at = %self.settings.at, next = %next, "next encrypted backup scheduled");
            tokio::select! {
                () = shutdown.cancelled() => break,
                () = tokio::time::sleep(wait) => {}
            }
            match self.run_once().await {
                Ok(run) => tracing::info!(
                    object_key = %run.report.object_key,
                    bytes = run.report.archive_bytes,
                    "backup snapshot ready; uploading in the background"
                ),
                Err(error) => tracing::error!(error = %error, "encrypted backup failed"),
            }
        }
    }

    /// Takes one backup. Returns once the archive is encrypted and the upload
    /// task is running; the app is serving again by then.
    pub async fn run_once(&self) -> Result<BackupRun, BackupError> {
        let _busy = self.busy.try_lock().map_err(|_| BackupError::Busy)?;
        let directory = self.directory();
        std::fs::create_dir_all(&directory)
            .with_context(|| format!("creating {}", directory.display()))?;
        self.clear_leftovers(&directory);

        let stamp = utc_stamp(Timestamp::now().as_second());
        let name = format!("backup-{stamp}-{}", random_suffix());
        let snapshot = directory.join(format!("{name}.db"));
        let archive_path = directory.join(format!("{name}.tar.gz"));
        let encrypted = directory.join(format!("{name}.tar.gz.age"));

        self.snapshot(&snapshot).await?;
        let counts = self.store.backup_counts().await?;

        let (data, bytes, digest) = self.package(&snapshot, &archive_path, &encrypted).await?;
        for path in [&snapshot, &archive_path, &encrypted] {
            let _ = std::fs::remove_file(path);
        }

        let object_key = format!("{}{name}.volume.tar.gz.age", self.settings.prefix);
        let report = BackupReport {
            format: 1,
            created_at: stamp,
            engine: format!("{:?}", self.store.backend()).to_ascii_lowercase(),
            database: self.settings.database_path.display().to_string(),
            object_key: object_key.clone(),
            archive_sha256: digest,
            archive_bytes: bytes,
            verified: Verified {
                method: "vacuum into".to_owned(),
                monitors: counts.monitors,
                status_pages: counts.status_pages,
                check_results: counts.check_results,
            },
            secrets_key: SECRETS_KEY.to_owned(),
        };

        let s3 = self.s3.clone();
        let report_key = format!("{}{name}.report.json", self.settings.prefix);
        let report_json = serde_json::to_vec_pretty(&report).context("serializing the report")?;
        let upload = tokio::spawn(async move {
            s3.put(&object_key, "application/octet-stream", &data)
                .await
                .context("uploading the encrypted archive")?;
            s3.put(&report_key, "application/json", &report_json)
                .await
                .context("uploading the backup report")?;
            tracing::info!(
                key = %object_key,
                bytes = data.len(),
                "encrypted backup uploaded"
            );
            Ok(())
        });

        Ok(BackupRun { report, upload })
    }

    /// Snapshot under the write gate, resuming as soon as it finishes.
    async fn snapshot(&self, path: &Path) -> Result<(), BackupError> {
        let snapped = {
            let guard = self.store.pause_writes();
            let result =
                tokio::time::timeout(self.settings.write_timeout, self.store.snapshot_to(path))
                    .await;
            drop(guard);
            result
        };
        match snapped {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => {
                let _ = std::fs::remove_file(path);
                Err(error.into())
            }
            Err(_) => {
                let _ = std::fs::remove_file(path);
                Err(BackupError::SnapshotTimeout(self.settings.write_timeout))
            }
        }
    }

    /// Package the snapshot and uploads, encrypt to the age recipient, and read
    /// the ciphertext back. Returns the bytes, size and SHA-256.
    async fn package(
        &self,
        snapshot: &Path,
        archive_path: &Path,
        encrypted: &Path,
    ) -> Result<(Vec<u8>, u64, String), BackupError> {
        let (snapshot, archive_path, encrypted) = (
            snapshot.to_path_buf(),
            archive_path.to_path_buf(),
            encrypted.to_path_buf(),
        );
        let recipient = self.settings.recipient.clone();
        let uploads = self.settings.uploads.clone();
        let built = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let mut entries = vec![Entry {
                name: "db/uptimestatus.db",
                source: &snapshot,
                directory: false,
            }];
            if let Some(uploads) = &uploads
                && uploads.is_dir()
            {
                entries.push(Entry {
                    name: "images",
                    source: uploads,
                    directory: true,
                });
            }
            archive::pack(&entries, &archive_path)?;
            archive::encrypt(&recipient, &archive_path, &encrypted)?;
            let digest = archive::sha256_file(&encrypted)?;
            let bytes = std::fs::metadata(&encrypted)?.len();
            let data = std::fs::read(&encrypted)?;
            Ok((data, bytes, digest))
        })
        .await
        .map_err(anyhow::Error::from)??;
        Ok(built)
    }

    /// Where temporary snapshots and archives live: beside the database.
    fn directory(&self) -> PathBuf {
        self.settings
            .database_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(std::env::temp_dir)
    }

    /// Removes archives a previous run left behind.
    fn clear_leftovers(&self, directory: &Path) {
        let Ok(entries) = std::fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with("backup-")
                && (name.ends_with(".db")
                    || name.ends_with(".tar.gz")
                    || name.ends_with(".tar.gz.age"))
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// The next UTC occurrence of `at`, strictly after `now`.
pub(crate) fn next_run_at(at: crate::TimeOfDay, now: Timestamp) -> Timestamp {
    let seconds = now.as_second();
    let target = i64::from(at.hour) * 3600 + i64::from(at.minute) * 60;
    let today = seconds.rem_euclid(86_400);
    let delta = if target > today {
        target - today
    } else {
        86_400 - today + target
    };
    Timestamp::from_second(seconds + delta).unwrap_or(now)
}

/// Eight hex characters, so two runs in the same second never collide.
fn random_suffix() -> String {
    let mut bytes = [0u8; 4];
    if getrandom::fill(&mut bytes).is_err() {
        return format!("{:08x}", Timestamp::now().as_nanosecond());
    }
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TimeOfDay;
    use pretty_assertions::assert_eq;

    fn at(hour: u8, minute: u8) -> TimeOfDay {
        TimeOfDay { hour, minute }
    }

    #[test]
    fn a_time_later_today_schedules_today() {
        // 2026-10-15 01:00:00 UTC; 03:17 is later the same day.
        let now = Timestamp::from_second(1_792_026_000).unwrap();
        let next = next_run_at(at(3, 17), now).as_second();
        assert_eq!(next, now.as_second() + 2 * 3600 + 17 * 60);
    }

    #[test]
    fn a_time_already_passed_schedules_tomorrow() {
        let now = Timestamp::from_second(1_792_026_000).unwrap();
        let next = next_run_at(at(1, 0), now).as_second();
        assert_eq!(next, now.as_second() + 86_400);
    }

    #[test]
    fn a_time_exactly_now_schedules_tomorrow() {
        let now = Timestamp::from_second(1_792_026_000).unwrap();
        let next = next_run_at(at(1, 0), now).as_second();
        assert_eq!(next, now.as_second() + 86_400);
    }
}
