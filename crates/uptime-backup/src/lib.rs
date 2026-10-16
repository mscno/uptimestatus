//! Encrypted, off-volume backups of the local database to Fly Tigris.
//!
//! A run snapshots the database with `VACUUM INTO` (see
//! [`uptime_store::Store::snapshot_to`]), packages the snapshot and uploads
//! into a `tar.gz`, encrypts that to an age recipient, then hands the archive
//! to a background task that PUTs it to an S3-compatible bucket. Writes are
//! paused only for the snapshot; the network copy runs after the app resumes.

mod archive;
mod runner;
mod s3;

use std::{fmt, path::PathBuf, time::Duration};

pub use runner::{Backup, BackupReport, BackupRun, Verified};

/// An S3-compatible target (Fly Tigris).
#[derive(Clone)]
pub struct S3Settings {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub access_key_id: String,
    pub secret_access_key: String,
}

impl fmt::Debug for S3Settings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Settings")
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("bucket", &self.bucket)
            .field("access_key_id", &"***")
            .field("secret_access_key", &"***")
            .finish()
    }
}

/// Everything one backup run needs, resolved from configuration.
#[derive(Clone)]
pub struct BackupSettings {
    pub s3: S3Settings,
    /// age recipient (`age1…`) the archive is encrypted to.
    pub recipient: String,
    /// Object key prefix; a per-run name is appended to it.
    pub prefix: String,
    /// The local SQLite/Turso database to snapshot.
    pub database_path: PathBuf,
    /// Uploaded images to include, when storage is configured.
    pub uploads: Option<PathBuf>,
    /// How long writes may be paused; the snapshot must finish within it.
    pub write_timeout: Duration,
    /// UTC time of day a scheduled backup runs.
    pub at: TimeOfDay,
}

impl fmt::Debug for BackupSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BackupSettings")
            .field("s3", &self.s3)
            .field("recipient", &self.recipient)
            .field("prefix", &self.prefix)
            .field("database_path", &self.database_path)
            .field("uploads", &self.uploads)
            .field("write_timeout", &self.write_timeout)
            .field("at", &self.at)
            .finish()
    }
}

impl BackupSettings {
    /// Rejects the settings a backup cannot run without.
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.recipient.trim().starts_with("age1"),
            "backup.recipient must be an age recipient (age1…): the identity is kept off the machine"
        );
        anyhow::ensure!(
            !self.s3.endpoint.trim().is_empty(),
            "backup.endpoint is required"
        );
        anyhow::ensure!(
            !self.s3.bucket.trim().is_empty(),
            "backup.bucket is required"
        );
        anyhow::ensure!(
            !self.s3.access_key_id.trim().is_empty(),
            "backup.access_key_id is required (Fly sets AWS_ACCESS_KEY_ID)"
        );
        anyhow::ensure!(
            !self.s3.secret_access_key.trim().is_empty(),
            "backup.secret_access_key is required (Fly sets AWS_SECRET_ACCESS_KEY)"
        );
        Ok(())
    }
}

/// A time of day in UTC, as `HH:MM`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeOfDay {
    pub hour: u8,
    pub minute: u8,
}

impl TimeOfDay {
    /// Parses `HH:MM` (hours 0–23, minutes 0–59).
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let text = text.trim();
        let (hour, minute) = text
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("expected a time of day HH:MM, got `{text}`"))?;
        let hour: u8 = hour
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("`{text}` has an invalid hour"))?;
        let minute: u8 = minute
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("`{text}` has an invalid minute"))?;
        anyhow::ensure!(
            hour < 24 && minute < 60,
            "`{text}` is not a valid time of day"
        );
        Ok(Self { hour, minute })
    }
}

impl fmt::Display for TimeOfDay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02}:{:02}", self.hour, self.minute)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn times_of_day_parse_and_reject() {
        assert_eq!(
            TimeOfDay::parse("03:17").unwrap(),
            TimeOfDay {
                hour: 3,
                minute: 17
            }
        );
        assert_eq!(TimeOfDay::parse(" 23:59 ").unwrap().to_string(), "23:59");
        for bad in ["", "0317", "24:00", "12:60", "aa:bb", "12:"] {
            assert!(TimeOfDay::parse(bad).is_err(), "{bad} should be rejected");
        }
    }
}
