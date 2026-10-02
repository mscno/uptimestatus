//! Uploaded images (logos, favicons), stored on local disk: a persistent volume in
//! production. Files are named by content hash, so they never change and can
//! be cached forever.
//!
//! Uploads are recognised by their bytes, not the browser's claimed type, and
//! are served with a sandboxing Content-Security-Policy: an SVG opened directly
//! cannot run script on our origin.

use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use axum::{
    extract::Path as UrlPath,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use sha2::{Digest as _, Sha256};

/// Largest accepted image.
pub const MAX_IMAGE_BYTES: usize = 512 * 1024;

/// Why an upload was refused.
#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("The image is larger than 512 KB.")]
    TooLarge,
    #[error("Upload a PNG, JPEG, WebP, GIF, SVG or ICO image.")]
    Unsupported,
    #[error("storing the image failed: {0}")]
    Io(#[from] io::Error),
}

/// A recognised image format: file extension and media type.
fn sniff(bytes: &[u8]) -> Option<(&'static str, &'static str)> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(("png", "image/png"));
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some(("jpg", "image/jpeg"));
    }
    if bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some(("webp", "image/webp"));
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some(("gif", "image/gif"));
    }
    if bytes.starts_with(&[0, 0, 1, 0]) {
        return Some(("ico", "image/x-icon"));
    }
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(1024)]);
    let head = head.trim_start_matches('\u{feff}').trim_start();
    let looks_svg =
        (head.starts_with("<svg") || head.starts_with("<?xml") || head.starts_with("<!--"))
            && head.contains("<svg");
    looks_svg.then_some(("svg", "image/svg+xml"))
}

fn content_type(name: &str) -> Option<&'static str> {
    Some(match name.rsplit_once('.')?.1 {
        "png" => "image/png",
        "jpg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "ico" => "image/x-icon",
        "svg" => "image/svg+xml",
        _ => return None,
    })
}

/// Our own file names only: 32 hex characters and a known extension.
fn valid_name(name: &str) -> bool {
    match name.split_once('.') {
        Some((stem, ext)) => {
            stem.len() == 32
                && stem
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
                && content_type(&format!("x.{ext}")).is_some()
        }
        None => false,
    }
}

/// Images on disk under `<dir>/images`.
#[derive(Clone, Debug)]
pub struct MediaStore {
    dir: Arc<PathBuf>,
}

impl MediaStore {
    /// Uses (and creates) `<root>/images`.
    pub fn open(root: &Path) -> io::Result<Self> {
        let dir = root.join("images");
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir: Arc::new(dir) })
    }

    /// Stores an image and returns its file name. Saving the same bytes twice
    /// returns the same name.
    pub async fn save_image(&self, bytes: &[u8]) -> Result<String, MediaError> {
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(MediaError::TooLarge);
        }
        let (ext, _) = sniff(bytes).ok_or(MediaError::Unsupported)?;
        let digest = Sha256::digest(bytes);
        let stem: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
        let name = format!("{stem}.{ext}");
        let path = self.dir.join(&name);
        if tokio::fs::try_exists(&path).await? {
            return Ok(name);
        }
        let partial = self.dir.join(format!(".{name}.partial"));
        tokio::fs::write(&partial, bytes).await?;
        tokio::fs::rename(&partial, &path).await?;
        Ok(name)
    }

    /// The bytes of a stored image.
    pub async fn read(&self, name: &str) -> Option<Vec<u8>> {
        if !valid_name(name) {
            return None;
        }
        tokio::fs::read(self.dir.join(name)).await.ok()
    }

    /// Deletes a stored image (best effort: a missing file is fine).
    pub async fn delete(&self, name: &str) {
        if valid_name(name)
            && let Err(error) = tokio::fs::remove_file(self.dir.join(name)).await
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(error = %uptime_domain::Report(&error), name, "deleting an image failed");
        }
    }
}

/// The public URL of a stored image.
pub fn url(name: &str) -> String {
    format!("/media/{name}")
}

/// `GET /media/{name}`
pub(crate) async fn serve(media: Option<MediaStore>, UrlPath(name): UrlPath<String>) -> Response {
    let (Some(media), Some(kind)) = (media, content_type(&name)) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(bytes) = media.read(&name).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(kind)),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=31536000, immutable"),
            ),
            (
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            ),
            (
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'; sandbox"),
            ),
        ],
        bytes,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn images_are_recognised_by_their_bytes() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\nrest").unwrap().0, "png");
        assert_eq!(sniff(&[0xff, 0xd8, 0xff, 0xe0]).unwrap().0, "jpg");
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 ").unwrap().0, "webp");
        assert_eq!(sniff(b"GIF89a....").unwrap().0, "gif");
        assert_eq!(sniff(&[0, 0, 1, 0, 1, 0]).unwrap().0, "ico");
        assert_eq!(
            sniff(b"<?xml version=\"1.0\"?>\n<svg xmlns=\"x\"/>")
                .unwrap()
                .0,
            "svg"
        );
        assert_eq!(
            sniff(b"  <svg viewBox=\"0 0 1 1\"></svg>").unwrap().0,
            "svg"
        );
        assert_eq!(sniff(b"<html><script>alert(1)</script>"), None);
        assert_eq!(sniff(b"hello"), None);
    }

    #[test]
    fn only_our_own_names_are_served() {
        assert!(valid_name("0123456789abcdef0123456789abcdef.png"));
        assert!(!valid_name("../../etc/passwd"));
        assert!(!valid_name("0123456789ABCDEF0123456789ABCDEF.png"));
        assert!(!valid_name("0123456789abcdef0123456789abcdef.html"));
        assert!(!valid_name("short.png"));
    }
}
