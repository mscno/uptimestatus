//! Static files embedded in the binary and served under `/static/`.
//!
//! - `app.css`: Starbase's design tokens and themes followed by our styles, as
//!   one stylesheet.
//! - `datastar.js`: Datastar v1.0.4 with Rocket (MIT), from
//!   `vendor/datastar/`.
//! - `starbase/…`: vendored Starbase components (MIT) and fonts (OFL); see
//!   `static/starbase/README.md`.
//!
//! URLs carry a `?v=` content hash ([`url`]); such requests are cached for a
//! year. Files fetched without it (e.g. a module's relative imports) are
//! cached for an hour.

use std::{collections::HashMap, sync::LazyLock};

use axum::{
    extract::{Path, RawQuery},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use include_dir::{Dir, include_dir};

static STATIC: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/static");

const APP_CSS: &str = concat!(
    include_str!("../static/starbase/css/tokens.css"),
    include_str!("../static/starbase/css/theme.css"),
    include_str!("../static/starbase/css/daylight.css"),
    include_str!("../static/app.css"),
);
const DATASTAR_JS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/vendor/datastar/datastar-rocket.js"
));

const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const SHORT: &str = "public, max-age=3600";

/// `path` (relative to `/static/`) → content hash.
static HASHES: LazyLock<HashMap<String, String>> = LazyLock::new(|| {
    let mut hashes = HashMap::new();
    hashes.insert("app.css".to_owned(), hash(APP_CSS.as_bytes()));
    hashes.insert("datastar.js".to_owned(), hash(DATASTAR_JS.as_bytes()));
    let mut stack = vec![&STATIC];
    while let Some(dir) = stack.pop() {
        stack.extend(dir.dirs());
        for file in dir.files() {
            let path = file.path().to_string_lossy().into_owned();
            hashes.entry(path).or_insert_with(|| hash(file.contents()));
        }
    }
    hashes
});

/// The versioned URL of a static file, e.g. `url("app.css")` →
/// `/static/app.css?v=…`.
pub(crate) fn url(path: &str) -> String {
    match HASHES.get(path) {
        Some(hash) => format!("/static/{path}?v={hash}"),
        None => format!("/static/{path}"),
    }
}

pub(crate) static CSS_HREF: LazyLock<String> = LazyLock::new(|| url("app.css"));
pub(crate) static DATASTAR_SRC: LazyLock<String> = LazyLock::new(|| url("datastar.js"));

/// The import map: Rocket components `import … from "datastar"`, which must
/// resolve to the very same URL as the page's Datastar script (one runtime).
pub(crate) static IMPORT_MAP: LazyLock<String> = LazyLock::new(|| {
    serde_json::json!({ "imports": { "datastar": DATASTAR_SRC.as_str() } }).to_string()
});

/// A vendored Starbase component module, e.g. `component("button")`.
pub(crate) fn component(name: &str) -> String {
    url(&format!("starbase/c/{name}/{name}.min.js"))
}

/// `GET /static/{*path}`
pub(crate) async fn serve(Path(path): Path<String>, RawQuery(query): RawQuery) -> Response {
    let body: &'static [u8] = match path.as_str() {
        "app.css" => APP_CSS.as_bytes(),
        "datastar.js" => DATASTAR_JS.as_bytes(),
        other => match STATIC.get_file(other) {
            Some(file) => file.contents(),
            None => return StatusCode::NOT_FOUND.into_response(),
        },
    };
    let versioned = query
        .as_deref()
        .is_some_and(|q| q.split('&').any(|pair| pair.starts_with("v=")));
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static(content_type(&path)),
            ),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static(if versioned { IMMUTABLE } else { SHORT }),
            ),
            (
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            ),
        ],
        body,
    )
        .into_response()
}

fn content_type(path: &str) -> &'static str {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rsplit_once('.').map(|(_, ext)| ext) {
        Some("css") => "text/css; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("woff2") => "font/woff2",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("txt" | "md") => "text/plain; charset=utf-8",
        None if name == "LICENSE" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn hash(bytes: &[u8]) -> String {
    let fnv = bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    format!("{fnv:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_versioned_by_content() {
        assert!(url("app.css").starts_with("/static/app.css?v="));
        assert!(component("button").starts_with("/static/starbase/c/button/button.min.js?v="));
        assert_eq!(url("missing.js"), "/static/missing.js");
    }

    #[test]
    fn the_import_map_points_datastar_at_the_served_bundle() {
        let map: serde_json::Value = serde_json::from_str(&IMPORT_MAP).unwrap_or_default();
        assert_eq!(map["imports"]["datastar"], DATASTAR_SRC.as_str());
    }

    #[test]
    fn the_stylesheet_starts_with_starbase_tokens() {
        assert!(APP_CSS.starts_with("/*"));
        assert!(APP_CSS.contains("--sb-brand"));
        assert!(APP_CSS.contains("/static/starbase/fonts/pixelify-sans.woff2"));
    }

    #[test]
    fn content_types() {
        assert_eq!(content_type("a/b.min.js"), "text/javascript; charset=utf-8");
        assert_eq!(content_type("fonts/x.woff2"), "font/woff2");
        assert_eq!(
            content_type("starbase/LICENSE"),
            "text/plain; charset=utf-8"
        );
        assert_eq!(content_type("x.bin"), "application/octet-stream");
    }
}
