#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! Logos and favicons: upload, serve, show, remove.

mod common;

use axum::http::StatusCode;
use common::TestApp;
use pretty_assertions::assert_eq;
use uptime_domain::{PageSpec, Theme, Website};

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR tiny test image";
const SVG: &[u8] = b"<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 1 1\"><rect width=\"1\" height=\"1\"/></svg>";

async fn setup() -> (TestApp, i64) {
    let app = TestApp::new().await;
    let page = app
        .db
        .store()
        .create_page(&PageSpec {
            slug: "platform".parse().unwrap(),
            title: "PrivateNPM Status".into(),
            description: None,
            accent: None,
            theme: Theme::Auto,
            look: Default::default(),
            published: true,
            website: Some(Website {
                url: "https://privatenpm.com/".parse().unwrap(),
                label: None,
            }),
            sections: vec![],
        })
        .await
        .unwrap();
    (app, page.id)
}

/// The `/media/…` URL in `html` after `attr="`.
fn media_url(html: &str, attr: &str) -> String {
    let start = html
        .find(&format!(r#"{attr}="/media/"#))
        .expect("an image URL")
        + attr.len()
        + 2;
    let end = start + html[start..].find('"').unwrap();
    html[start..end].to_owned()
}

#[tokio::test]
async fn an_uploaded_logo_is_served_shown_and_used_as_favicon() {
    let (app, id) = setup().await;
    let mut admin = app.signed_in("alice", 1001).await;

    let reply = admin
        .post_file(
            &format!("/admin/pages/{id}/images/logo"),
            "image",
            "logo.png",
            PNG,
        )
        .await;

    assert_eq!(
        reply.location(),
        Some(format!("/admin/pages/{id}#images").as_str())
    );
    let page = app.client().get("/s/platform").await;
    let logo = media_url(&page.body, "src");
    assert_eq!(
        media_url(&page.body, "href"),
        logo,
        "the logo doubles as favicon"
    );
    let image = app.client().get(&logo).await;
    assert_eq!(image.status, StatusCode::OK);
    assert_eq!(image.headers["content-type"], "image/png");
    assert!(
        image.headers["cache-control"]
            .to_str()
            .unwrap()
            .contains("immutable")
    );
    assert!(
        image.headers["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("sandbox")
    );
    admin
        .get(&format!("/admin/pages/{id}"))
        .await
        .assert_contains(&logo);
}

#[tokio::test]
async fn a_favicon_takes_precedence_over_the_logo() {
    let (app, id) = setup().await;
    let mut admin = app.signed_in("alice", 1001).await;
    admin
        .post_file(
            &format!("/admin/pages/{id}/images/logo"),
            "image",
            "logo.png",
            PNG,
        )
        .await;
    admin
        .post_file(
            &format!("/admin/pages/{id}/images/favicon"),
            "image",
            "icon.svg",
            SVG,
        )
        .await;

    let page = app.client().get("/s/platform").await;

    let favicon = media_url(&page.body, "href");
    assert!(favicon.ends_with(".svg"), "{favicon}");
    assert!(media_url(&page.body, "src").ends_with(".png"));
    let svg = app.client().get(&favicon).await;
    assert_eq!(svg.headers["content-type"], "image/svg+xml");
    assert!(
        svg.headers["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("sandbox")
    );
}

#[tokio::test]
async fn non_images_and_empty_uploads_are_refused() {
    let (app, id) = setup().await;
    let mut admin = app.signed_in("alice", 1001).await;

    let html = admin
        .post_file(
            &format!("/admin/pages/{id}/images/logo"),
            "image",
            "evil.png",
            b"<html><script>alert(1)</script></html>",
        )
        .await;
    let empty = admin
        .post_file(
            &format!("/admin/pages/{id}/images/logo"),
            "image",
            "x.png",
            b"",
        )
        .await;

    assert_eq!(
        html.location(),
        Some(format!("/admin/pages/{id}?image_error=unsupported#images").as_str())
    );
    assert!(empty.location().unwrap().contains("image_error=missing"));
    admin
        .get(&format!("/admin/pages/{id}?image_error=unsupported"))
        .await
        .assert_contains("Upload a PNG, JPEG, WebP, GIF, SVG or ICO image.");
    assert_eq!(app.db.store().page(id).await.unwrap().unwrap().logo, None);
}

#[tokio::test]
async fn oversized_images_are_refused() {
    let (app, id) = setup().await;
    let mut big = PNG.to_vec();
    big.resize(600 * 1024, 0);

    let reply = app
        .signed_in("alice", 1001)
        .await
        .post_file(
            &format!("/admin/pages/{id}/images/logo"),
            "image",
            "big.png",
            &big,
        )
        .await;

    assert!(reply.location().unwrap().contains("image_error=too_large"));
}

#[tokio::test]
async fn removing_an_image_deletes_the_unused_file() {
    let (app, id) = setup().await;
    let mut admin = app.signed_in("alice", 1001).await;
    admin
        .post_file(
            &format!("/admin/pages/{id}/images/logo"),
            "image",
            "logo.png",
            PNG,
        )
        .await;
    let logo = media_url(&app.client().get("/s/platform").await.body, "src");

    admin
        .post_form(&format!("/admin/pages/{id}/images/logo/delete"), &[])
        .await;

    assert_eq!(app.db.store().page(id).await.unwrap().unwrap().logo, None);
    assert_eq!(app.client().get(&logo).await.status, StatusCode::NOT_FOUND);
    let page = app.client().get("/s/platform").await;
    assert!(
        !page.body.contains("/media/"),
        "no logo, no favicon override"
    );
}

#[tokio::test]
async fn custom_domains_load_the_logo_too() {
    let (app, id) = setup().await;
    app.signed_in("alice", 1001)
        .await
        .post_file(
            &format!("/admin/pages/{id}/images/logo"),
            "image",
            "logo.png",
            PNG,
        )
        .await;
    let logo = media_url(&app.client().get("/s/platform").await.body, "src");
    app.state
        .domains
        .replace([("status.privatenpm.com".to_owned(), "platform".to_owned())]);

    let mut visitor = app.client();
    let home = visitor.get_on("status.privatenpm.com", "/").await;
    let image = visitor.get_on("status.privatenpm.com", &logo).await;

    home.assert_contains(&logo);
    assert_eq!(image.status, StatusCode::OK);
}

#[tokio::test]
async fn uploads_require_signing_in_and_a_known_slot() {
    let (app, id) = setup().await;
    let anonymous = app
        .client()
        .post_file(
            &format!("/admin/pages/{id}/images/logo"),
            "image",
            "logo.png",
            PNG,
        )
        .await;
    assert_ne!(anonymous.status, StatusCode::SEE_OTHER);
    assert_eq!(app.db.store().page(id).await.unwrap().unwrap().logo, None);

    let unknown = app
        .signed_in("alice", 1001)
        .await
        .post_file(
            &format!("/admin/pages/{id}/images/banner"),
            "image",
            "x.png",
            PNG,
        )
        .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn status_pages_link_back_to_the_product() {
    let (app, _) = setup().await;
    let page = app.client().get("/s/platform").await;
    page.assert_contains(r#"href="https://privatenpm.com/""#)
        .assert_contains("Back to privatenpm.com")
        .assert_contains("sb-starfield");
}
