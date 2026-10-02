#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! Signing in with GitHub, the allowlist, and sessions.

mod common;

use axum::http::StatusCode;
use common::TestApp;
use pretty_assertions::assert_eq;
use uptime_web::auth::Key;

#[tokio::test]
async fn the_console_requires_signing_in() {
    let app = TestApp::new().await;
    let reply = app.client().get("/admin").await;
    assert_eq!(reply.status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(reply.location(), Some("/login"));
}

#[tokio::test]
async fn the_app_root_goes_to_the_console() {
    let app = TestApp::new().await;
    let reply = app.client().get("/").await;
    assert_eq!(reply.location(), Some("/admin"));
}

#[tokio::test]
async fn the_login_page_offers_github() {
    let app = TestApp::new().await;
    let reply = app.client().get("/login").await;
    assert_eq!(reply.status, StatusCode::OK);
    reply
        .assert_contains("Sign in with GitHub")
        .assert_contains(r#"href="/auth/github""#);
}

#[tokio::test]
async fn starting_the_login_redirects_to_github_with_pkce() {
    let app = TestApp::new().await;
    let reply = app.client().get("/auth/github").await;

    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    let location: url::Url = reply.location().unwrap().parse().unwrap();
    let query: std::collections::HashMap<_, _> = location.query_pairs().into_owned().collect();
    assert_eq!(
        query["redirect_uri"],
        "https://status.example.com/auth/github/callback"
    );
    assert_eq!(query["code_challenge_method"], "S256");
    assert_eq!(query["code_challenge"].len(), 43, "base64url of a SHA-256");
    assert!(query["state"].len() >= 22);
}

#[tokio::test]
async fn an_allowlisted_user_signs_in_and_sees_the_console() {
    let app = TestApp::new().await;
    let mut client = app.signed_in("Alice", 1001).await;

    let reply = client.get("/admin").await;

    assert_eq!(reply.status, StatusCode::OK);
    reply.assert_contains("alice").assert_contains("Monitors");
    assert_eq!(
        app.db.store().pinned_github_id("alice").await.unwrap(),
        Some(1001)
    );
}

#[tokio::test]
async fn users_off_the_allowlist_are_turned_away() {
    let app = TestApp::new().await;
    app.github_signs_in("mallory", 666).await;
    let mut client = app.client();

    let reply = client.oauth_round_trip().await;

    assert_eq!(reply.location(), Some("/login?error=denied"));
    assert_eq!(client.get("/admin").await.location(), Some("/login"));
}

#[tokio::test]
async fn a_reregistered_username_is_turned_away() {
    let app = TestApp::new().await;
    app.signed_in("alice", 1001).await;

    // Someone else now holds the username "alice".
    app.github_signs_in("alice", 2002).await;
    let mut impostor = app.client();
    let reply = impostor.oauth_round_trip().await;

    assert_eq!(reply.location(), Some("/login?error=denied"));
}

#[tokio::test]
async fn a_forged_or_stale_state_is_rejected() {
    let app = TestApp::new().await;
    app.github_signs_in("alice", 1001).await;
    let mut client = app.client();
    client.get("/auth/github").await;

    let reply = client
        .get("/auth/github/callback?code=the-code&state=not-the-state")
        .await;

    assert_eq!(reply.location(), Some("/login?error=expired"));
    assert_eq!(client.get("/admin").await.location(), Some("/login"));
}

#[tokio::test]
async fn a_callback_without_the_flow_cookie_is_rejected() {
    let app = TestApp::new().await;
    let reply = app
        .client()
        .get("/auth/github/callback?code=x&state=y")
        .await;
    assert_eq!(reply.location(), Some("/login?error=expired"));
}

#[tokio::test]
async fn github_failures_are_reported() {
    let app = TestApp::new().await;
    app.github.reset().await; // GitHub answers nothing useful.
    let reply = app.client().oauth_round_trip().await;
    assert_eq!(reply.location(), Some("/login?error=github"));
}

#[tokio::test]
async fn signing_out_ends_the_session() {
    let app = TestApp::new().await;
    let mut client = app.signed_in("alice", 1001).await;

    let reply = client.post_form("/auth/logout", &[]).await;

    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location(), Some("/login"));
    assert_eq!(client.get("/admin").await.location(), Some("/login"));
}

#[tokio::test]
async fn removing_a_user_from_the_allowlist_revokes_their_sessions() {
    let app = TestApp::new().await;
    let key = Key::generate();
    let state = TestApp::state(&app.db, &app.github, "alice", key.clone());
    let mut client = common::Client::with_state(state, app.signed_in("alice", 1001).await.cookies);
    assert_eq!(client.get("/admin").await.status, StatusCode::OK);

    let restarted = TestApp::state(&app.db, &app.github, "bob", key);
    let mut client = common::Client::with_state(restarted, client.cookies);

    assert_eq!(client.get("/admin").await.location(), Some("/login"));
}

#[tokio::test]
async fn the_login_page_explains_errors() {
    let app = TestApp::new().await;
    let reply = app.client().get("/login?error=denied").await;
    reply.assert_contains("isn't on the admin list");
}
