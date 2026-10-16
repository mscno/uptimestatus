//! A minimal S3 client: one SigV4-signed `PutObject`, enough for cold backups.
//!
//! Fly Tigris is S3-compatible. The app holds only the bucket credentials, and
//! uploads are encrypted before they leave the machine. The signing core is
//! checked against AWS's documented example in the tests below.

use std::time::Duration;

use anyhow::Context as _;
use hmac::{Hmac, KeyInit as _, Mac as _};
use sha2::{Digest as _, Sha256};
use url::Url;

use crate::S3Settings;

type HmacSha256 = Hmac<Sha256>;

/// Transient upload failures are retried this many times.
const ATTEMPTS: u32 = 3;

/// An S3-compatible object store, used only for backups.
#[derive(Clone, Debug)]
pub(crate) struct S3Client {
    http: reqwest::Client,
    endpoint: Url,
    host: String,
    region: String,
    bucket: String,
    access_key_id: String,
    secret_access_key: String,
}

impl S3Client {
    pub(crate) fn new(settings: &S3Settings) -> anyhow::Result<Self> {
        let endpoint = Url::parse(settings.endpoint.trim())
            .with_context(|| format!("backup.endpoint `{}` is not a URL", settings.endpoint))?;
        anyhow::ensure!(
            matches!(endpoint.scheme(), "http" | "https"),
            "backup.endpoint must use http or https"
        );
        anyhow::ensure!(
            !settings.bucket.trim().is_empty(),
            "backup.bucket is required"
        );
        let host = match endpoint.host_str() {
            Some(host) => match endpoint.port() {
                Some(port) => format!("{host}:{port}"),
                None => host.to_owned(),
            },
            None => anyhow::bail!("backup.endpoint has no host"),
        };
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .context("building the backup HTTP client")?;
        Ok(Self {
            http,
            endpoint,
            host,
            region: settings.region.trim().to_owned(),
            bucket: settings.bucket.trim().to_owned(),
            access_key_id: settings.access_key_id.trim().to_owned(),
            secret_access_key: settings.secret_access_key.trim().to_owned(),
        })
    }

    /// Uploads `body` to `<endpoint>/<bucket>/<key>`, retrying transient errors.
    pub(crate) async fn put(
        &self,
        key: &str,
        content_type: &str,
        body: &[u8],
    ) -> anyhow::Result<()> {
        let payload = hex::encode(Sha256::digest(body));
        let url = self.object_url(key);
        let uri = url.path().to_owned();
        let mut last: Option<anyhow::Error> = None;
        for attempt in 1..=ATTEMPTS {
            let amz_date = utc_stamp(jiff::Timestamp::now().as_second());
            let authorization = sign(
                "PUT",
                &uri,
                &[
                    ("content-type".to_owned(), content_type.to_owned()),
                    ("host".to_owned(), self.host.clone()),
                    ("x-amz-content-sha256".to_owned(), payload.clone()),
                    ("x-amz-date".to_owned(), amz_date.clone()),
                ],
                &payload,
                &amz_date,
                &self.region,
                &self.access_key_id,
                &self.secret_access_key,
            );
            let sent = self
                .http
                .put(url.clone())
                .header("content-type", content_type)
                .header("x-amz-content-sha256", &payload)
                .header("x-amz-date", &amz_date)
                .header("authorization", authorization)
                .body(body.to_vec())
                .send()
                .await;
            match sent {
                Ok(response) if response.status().is_success() => return Ok(()),
                Ok(response) => {
                    let status = response.status();
                    let detail = response.text().await.unwrap_or_default();
                    last = Some(anyhow::anyhow!(
                        "uploading {key} failed: {status} {}",
                        detail.trim()
                    ));
                }
                Err(error) => {
                    last = Some(anyhow::Error::new(error).context(format!("uploading {key}")));
                }
            }
            if attempt < ATTEMPTS {
                tokio::time::sleep(Duration::from_secs(2 * u64::from(attempt))).await;
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("uploading {key} failed")))
    }

    /// The path-style object URL: `<endpoint>/<bucket>/<key>`.
    fn object_url(&self, key: &str) -> Url {
        let mut url = self.endpoint.clone();
        let base = url.path().trim_end_matches('/').to_owned();
        url.set_path(&format!("{base}/{}/{}", self.bucket, key));
        url
    }
}

/// Builds the `Authorization` header for one request. Headers are lowercase
/// names; the query string is always empty (none of our requests carry one).
#[allow(clippy::too_many_arguments)]
fn sign(
    method: &str,
    uri: &str,
    headers: &[(String, String)],
    payload_hash: &str,
    amz_date: &str,
    region: &str,
    access_key_id: &str,
    secret_access_key: &str,
) -> String {
    let mut headers: Vec<(String, String)> = headers
        .iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    headers.sort_by(|a, b| a.0.cmp(&b.0));
    let canonical_headers: String = headers
        .iter()
        .map(|(name, value)| format!("{name}:{value}\n"))
        .collect();
    let signed_headers = headers
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical_request =
        format!("{method}\n{uri}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}");
    let date = amz_date.get(..8).unwrap_or(amz_date);
    let scope = format!("{date}/{region}/s3/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        hex::encode(Sha256::digest(canonical_request.as_bytes()))
    );
    let signature = hex::encode(hmac_sha256(
        &signing_key(secret_access_key, date, region),
        string_to_sign.as_bytes(),
    ));
    format!(
        "AWS4-HMAC-SHA256 Credential={access_key_id}/{scope},\
         SignedHeaders={signed_headers},Signature={signature}"
    )
}

/// `HMAC-SHA256` (the key may be any length, so this never fails).
fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let Ok(mut mac) = HmacSha256::new_from_slice(key) else {
        return Vec::new();
    };
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn signing_key(secret: &str, date: &str, region: &str) -> Vec<u8> {
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, b"s3");
    hmac_sha256(&k_service, b"aws4_request")
}

/// `YYYYMMDDTHHMMSSZ` for a Unix timestamp, in UTC.
pub(crate) fn utc_stamp(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let time = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (time / 3600, (time % 3600) / 60, time % 60);
    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z")
}

/// Civil year, month and day from days since 1970-01-01 (Howard Hinnant's
/// `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = u64::try_from(z - era * 146_097).unwrap_or(0);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = i64::try_from(yoe).unwrap_or(0) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// AWS's documented `GET Object` example ("Signature Calculations for the
    /// Authorization Header"). The expected signature is a published value.
    #[test]
    fn signing_matches_the_documented_aws_example() {
        let empty = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let authorization = sign(
            "GET",
            "/test.txt",
            &[
                (
                    "host".to_owned(),
                    "examplebucket.s3.amazonaws.com".to_owned(),
                ),
                ("range".to_owned(), "bytes=0-9".to_owned()),
                ("x-amz-content-sha256".to_owned(), empty.to_owned()),
                ("x-amz-date".to_owned(), "20130524T000000Z".to_owned()),
            ],
            empty,
            "20130524T000000Z",
            "us-east-1",
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        );
        assert_eq!(
            authorization,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request,\
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date,\
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    #[test]
    fn amazon_dates_are_utc() {
        assert_eq!(utc_stamp(1_369_353_600), "20130524T000000Z");
        assert_eq!(utc_stamp(0), "19700101T000000Z");
    }

    #[test]
    fn object_urls_are_path_style() {
        let client = S3Client::new(&S3Settings {
            endpoint: "https://fly.storage.tigris.dev".into(),
            region: "auto".into(),
            bucket: "uptimestatus-backups".into(),
            access_key_id: "id".into(),
            secret_access_key: "secret".into(),
        })
        .unwrap();
        assert_eq!(
            client.object_url("backups/db.tar.gz.age").as_str(),
            "https://fly.storage.tigris.dev/uptimestatus-backups/backups/db.tar.gz.age"
        );
    }
}
