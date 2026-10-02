#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

mod common;

use common::t;
use jiff::SignedDuration;
use pretty_assertions::assert_eq;
use uptime_store::{GithubIdentity, NewSession};
use uptime_testkit::TestDb;

fn alice() -> GithubIdentity {
    GithubIdentity {
        github_id: 1001,
        login: "Alice".into(),
        name: Some("Alice A.".into()),
        avatar_url: Some("https://avatars.example/1001".into()),
    }
}

fn session(hash: u8, user_id: i64, created: i64, expires: i64) -> NewSession {
    NewSession {
        token_hash: [hash; 32],
        user_id,
        created_at: t(created),
        expires_at: t(expires),
        user_agent: Some("test-agent".into()),
    }
}

#[tokio::test]
async fn first_login_creates_the_admin_and_pins_the_github_id() {
    let db = TestDb::new().await;

    let admin = db.store().record_admin_login(&alice(), t(0)).await.unwrap();

    assert_eq!(admin.login, "alice", "logins are stored lowercase");
    assert_eq!(admin.github_id, 1001);
    assert_eq!(admin.name.as_deref(), Some("Alice A."));
    assert_eq!(
        db.store().pinned_github_id("ALICE").await.unwrap(),
        Some(1001)
    );
    assert_eq!(db.store().pinned_github_id("bob").await.unwrap(), None);
}

#[tokio::test]
async fn later_logins_refresh_the_profile_and_follow_renames() {
    let db = TestDb::new().await;
    let first = db.store().record_admin_login(&alice(), t(0)).await.unwrap();

    let renamed = GithubIdentity {
        login: "alice-new".into(),
        name: None,
        ..alice()
    };
    let again = db
        .store()
        .record_admin_login(&renamed, t(60))
        .await
        .unwrap();

    assert_eq!(again.id, first.id, "same GitHub account, same admin");
    assert_eq!(again.login, "alice-new");
    assert_eq!(again.name, None);
    assert_eq!(
        db.store().pinned_github_id("alice").await.unwrap(),
        None,
        "old name released"
    );
}

#[tokio::test]
async fn sessions_resolve_to_their_admin_until_they_expire() {
    let db = TestDb::new().await;
    let admin = db.store().record_admin_login(&alice(), t(0)).await.unwrap();
    db.store()
        .create_session(&session(7, admin.id, 0, 3600))
        .await
        .unwrap();

    let active = db
        .store()
        .session_admin(&[7; 32], t(10), SignedDuration::from_hours(720))
        .await
        .unwrap();
    let expired = db
        .store()
        .session_admin(&[7; 32], t(3600), SignedDuration::from_hours(720))
        .await
        .unwrap();
    let unknown = db
        .store()
        .session_admin(&[8; 32], t(10), SignedDuration::from_hours(720))
        .await
        .unwrap();

    let (found, info) = active.expect("session is active");
    assert_eq!(found, admin);
    assert_eq!(info.expires_at, t(3600));
    assert_eq!(expired, None);
    assert_eq!(unknown, None);
}

#[tokio::test]
async fn sessions_have_an_absolute_lifetime_even_when_extended() {
    let db = TestDb::new().await;
    let admin = db.store().record_admin_login(&alice(), t(0)).await.unwrap();
    db.store()
        .create_session(&session(7, admin.id, 0, 3600))
        .await
        .unwrap();
    db.store()
        .extend_session(&[7; 32], t(10_000), t(3000))
        .await
        .unwrap();

    let max_age = SignedDuration::from_secs(5000);
    assert!(
        db.store()
            .session_admin(&[7; 32], t(4999), max_age)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        db.store()
            .session_admin(&[7; 32], t(5000), max_age)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn logging_out_deletes_the_session() {
    let db = TestDb::new().await;
    let admin = db.store().record_admin_login(&alice(), t(0)).await.unwrap();
    db.store()
        .create_session(&session(7, admin.id, 0, 3600))
        .await
        .unwrap();

    db.store().delete_session(&[7; 32]).await.unwrap();

    assert_eq!(
        db.store()
            .session_admin(&[7; 32], t(1), SignedDuration::from_hours(1))
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn expired_sessions_are_pruned() {
    let db = TestDb::new().await;
    let admin = db.store().record_admin_login(&alice(), t(0)).await.unwrap();
    db.store()
        .create_session(&session(1, admin.id, 0, 100))
        .await
        .unwrap();
    db.store()
        .create_session(&session(2, admin.id, 0, 1000))
        .await
        .unwrap();

    assert_eq!(db.store().prune_sessions(t(500)).await.unwrap(), 1);
    assert!(
        db.store()
            .session_admin(&[2; 32], t(500), SignedDuration::from_hours(1))
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn api_tokens_authenticate_by_hash_and_can_be_revoked() {
    use uptime_domain::TokenScope;
    use uptime_store::NewApiToken;

    let db = TestDb::new().await;
    let store = db.store();
    let hash = [7u8; 32];
    let created = store
        .create_api_token(
            &NewApiToken {
                name: " terraform ".into(),
                token_hash: hash,
                scope: TokenScope::Write,
                created_by: "alice".into(),
            },
            t(100),
        )
        .await
        .unwrap();
    assert_eq!(created.name, "terraform");
    assert_eq!(created.last_used_at, None);

    let found = store
        .authenticate_api_token(&hash, t(200))
        .await
        .unwrap()
        .unwrap();
    assert_eq!((found.id, found.scope), (created.id, TokenScope::Write));
    assert!(
        store
            .authenticate_api_token(&[8u8; 32], t(200))
            .await
            .unwrap()
            .is_none()
    );

    let listed = store.list_api_tokens().await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].last_used_at, Some(t(200)));
    // A second use within the throttle window does not rewrite it.
    store.authenticate_api_token(&hash, t(230)).await.unwrap();
    assert_eq!(
        store.list_api_tokens().await.unwrap()[0].last_used_at,
        Some(t(200))
    );

    assert!(store.delete_api_token(created.id).await.unwrap());
    assert!(!store.delete_api_token(created.id).await.unwrap());
    assert!(
        store
            .authenticate_api_token(&hash, t(300))
            .await
            .unwrap()
            .is_none()
    );
}
