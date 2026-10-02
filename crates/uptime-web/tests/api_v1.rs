#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! The JSON API: bearer tokens, monitors, pages, incidents, and the console
//! page that issues tokens.

mod common;

use axum::http::StatusCode;
use common::TestApp;
use jiff::Timestamp;
use serde_json::{Value, json};
use uptime_domain::TokenScope;
use uptime_store::NewApiToken;

async fn token(app: &TestApp, scope: TokenScope) -> String {
    let secret = uptime_web::new_api_token();
    app.db
        .store()
        .create_api_token(
            &NewApiToken {
                name: "test".into(),
                token_hash: uptime_web::hash_api_token(&secret),
                scope,
                created_by: "alice".into(),
            },
            Timestamp::now(),
        )
        .await
        .unwrap();
    secret
}

fn json_of(reply: &common::Reply) -> Value {
    serde_json::from_str(&reply.body).unwrap_or_else(|e| panic!("{e}: {}", reply.body))
}

fn monitor(name: &str) -> Value {
    json!({
        "name": name,
        "check": {"type": "tcp", "host": "db.example.com", "port": 5432},
        "tags": ["Prod"],
    })
}

#[tokio::test]
async fn requests_need_a_valid_token_with_enough_scope() {
    let app = TestApp::new().await;
    let read = token(&app, TokenScope::Read).await;
    let mut client = app.client();

    let anonymous = client.api("GET", "/api/v1/monitors", None, None).await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
    assert_eq!(anonymous.headers["www-authenticate"], "Bearer");
    let bogus = client
        .api("GET", "/api/v1/monitors", Some("upt_nope"), None)
        .await;
    assert_eq!(bogus.status, StatusCode::UNAUTHORIZED);
    let ok = client
        .api("GET", "/api/v1/monitors", Some(&read), None)
        .await;
    assert_eq!(ok.status, StatusCode::OK);
    assert_eq!(ok.headers["cache-control"], "no-store");
    assert_eq!(json_of(&ok)["monitors"], json!([]));

    let denied = client
        .api(
            "PUT",
            "/api/v1/monitors/db",
            Some(&read),
            Some(monitor("DB")),
        )
        .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    assert!(app.db.store().list_monitors().await.unwrap().is_empty());
}

#[tokio::test]
async fn monitors_are_upserted_listed_paused_and_deleted() {
    let app = TestApp::new().await;
    let write = token(&app, TokenScope::Write).await;
    let mut client = app.client();

    let created = client
        .api(
            "PUT",
            "/api/v1/monitors/db",
            Some(&write),
            Some(monitor("Database")),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let body = json_of(&created);
    assert_eq!(body["key"], "db");
    assert_eq!(body["tags"], json!(["prod"]));
    assert_eq!(body["state"], "unknown");

    let updated = client
        .api(
            "PUT",
            "/api/v1/monitors/db",
            Some(&write),
            Some(monitor("Primary database")),
        )
        .await;
    assert_eq!(updated.status, StatusCode::OK);
    assert_eq!(app.db.store().list_monitors().await.unwrap().len(), 1);

    let found = client
        .api(
            "GET",
            "/api/v1/monitors?tag=prod&q=primary",
            Some(&write),
            None,
        )
        .await;
    assert_eq!(json_of(&found)["monitors"][0]["name"], "Primary database");
    let none = client
        .api("GET", "/api/v1/monitors?tag=staging", Some(&write), None)
        .await;
    assert_eq!(json_of(&none)["monitors"], json!([]));

    let paused = client
        .api("POST", "/api/v1/monitors/db/pause", Some(&write), None)
        .await;
    assert_eq!(paused.status, StatusCode::NO_CONTENT);
    let one = client
        .api("GET", "/api/v1/monitors/db", Some(&write), None)
        .await;
    assert_eq!(json_of(&one)["active"], false);

    let checks = client
        .api(
            "GET",
            "/api/v1/monitors/db/checks?limit=5",
            Some(&write),
            None,
        )
        .await;
    assert_eq!(json_of(&checks)["checks"], json!([]));

    let gone = client
        .api("DELETE", "/api/v1/monitors/db", Some(&write), None)
        .await;
    assert_eq!(gone.status, StatusCode::NO_CONTENT);
    let missing = client
        .api("GET", "/api/v1/monitors/db", Some(&write), None)
        .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn bad_bodies_are_explained() {
    let app = TestApp::new().await;
    let write = token(&app, TokenScope::Write).await;
    let mut client = app.client();

    let mismatched = client
        .api(
            "PUT",
            "/api/v1/monitors/db",
            Some(&write),
            Some(json!({"key": "other", "name": "x", "check": {"type": "tcp", "host": "h", "port": 1}})),
        )
        .await;
    assert_eq!(mismatched.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        json_of(&mismatched)["error"]
            .as_str()
            .unwrap()
            .contains("match the URL")
    );

    let invalid = client
        .api(
            "PUT",
            "/api/v1/monitors/db",
            Some(&write),
            Some(json!({"name": "x"})),
        )
        .await;
    assert_eq!(invalid.status, StatusCode::UNPROCESSABLE_ENTITY);

    let bad_tag = client
        .api(
            "PUT",
            "/api/v1/monitors/db",
            Some(&write),
            Some(json!({"name": "x", "tags": ["no way!"], "check": {"type": "tcp", "host": "h", "port": 1}})),
        )
        .await;
    assert_eq!(bad_tag.status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn read_tokens_see_secrets_redacted_and_write_tokens_do_not() {
    let app = TestApp::new().await;
    let read = token(&app, TokenScope::Read).await;
    let write = token(&app, TokenScope::Write).await;
    let mut client = app.client();
    client
        .api(
            "PUT",
            "/api/v1/monitors/api",
            Some(&write),
            Some(json!({
                "name": "API",
                "check": {
                    "type": "http",
                    "url": "https://url-user:url-password@example.com/secret-path?credential=query-secret",
                    "headers": [["X-Custom", "header-secret"]],
                    "body": "body-secret",
                    "auth": {"scheme": "bearer", "token": "s3cr3t-token"}
                }
            })),
        )
        .await;
    client
        .api(
            "PUT",
            "/api/v1/monitors/tcp",
            Some(&write),
            Some(json!({
                "name": "TCP",
                "check": {
                    "type": "tcp",
                    "host": "example.com",
                    "port": 443,
                    "send": "tcp-send-secret",
                    "expect": "tcp-expect-secret"
                }
            })),
        )
        .await;

    for path in [
        "/api/v1/config",
        "/api/v1/monitors",
        "/api/v1/monitors/api",
        "/api/v1/monitors/tcp",
    ] {
        let as_read = client.api("GET", path, Some(&read), None).await;
        assert_eq!(as_read.status, StatusCode::OK);
        for secret in [
            "s3cr3t-token",
            "url-user",
            "url-password",
            "secret-path",
            "query-secret",
            "header-secret",
            "body-secret",
            "tcp-send-secret",
            "tcp-expect-secret",
        ] {
            assert!(
                !as_read.body.contains(secret),
                "{path} leaked {secret}: {}",
                as_read.body
            );
        }
    }
    let as_write = client
        .api("GET", "/api/v1/config", Some(&write), None)
        .await;
    for secret in [
        "s3cr3t-token",
        "query-secret",
        "header-secret",
        "body-secret",
    ] {
        assert!(as_write.body.contains(secret));
    }
}

#[tokio::test]
async fn pages_are_upserted_and_validated_against_monitors() {
    let app = TestApp::new().await;
    let write = token(&app, TokenScope::Write).await;
    let mut client = app.client();
    let page = json!({
        "title": "Status",
        "section": [{"name": "Core", "components": [{"monitor": "db"}]}]
    });

    let orphan = client
        .api(
            "PUT",
            "/api/v1/pages/status",
            Some(&write),
            Some(page.clone()),
        )
        .await;
    assert_eq!(
        orphan.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        orphan.body
    );

    client
        .api(
            "PUT",
            "/api/v1/monitors/db",
            Some(&write),
            Some(monitor("DB")),
        )
        .await;
    let created = client
        .api(
            "PUT",
            "/api/v1/pages/status",
            Some(&write),
            Some(page.clone()),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let updated = client
        .api("PUT", "/api/v1/pages/status", Some(&write), Some(page))
        .await;
    assert_eq!(updated.status, StatusCode::OK);
    let fetched = client
        .api("GET", "/api/v1/pages/status", Some(&write), None)
        .await;
    assert_eq!(json_of(&fetched)["title"], "Status");
    app.client()
        .get("/s/status")
        .await
        .assert_contains("Status");

    let deleted = client
        .api("DELETE", "/api/v1/pages/status", Some(&write), None)
        .await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    assert_eq!(
        app.client().get("/s/status").await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn incidents_can_be_declared_and_updated() {
    let app = TestApp::new().await;
    let write = token(&app, TokenScope::Write).await;
    let mut client = app.client();
    client
        .api(
            "PUT",
            "/api/v1/monitors/db",
            Some(&write),
            Some(monitor("DB")),
        )
        .await;

    let declared = client
        .api(
            "POST",
            "/api/v1/incidents",
            Some(&write),
            Some(json!({"title": "DB slow", "message": "Investigating.", "impact": "major", "monitors": ["db"]})),
        )
        .await;
    assert_eq!(declared.status, StatusCode::CREATED, "{}", declared.body);
    let id = json_of(&declared)["id"].as_i64().unwrap();
    assert_eq!(json_of(&declared)["status"], "investigating");

    let resolved = client
        .api(
            "POST",
            &format!("/api/v1/incidents/{id}/updates"),
            Some(&write),
            Some(json!({"status": "resolved", "message": "Fixed."})),
        )
        .await;
    assert_eq!(resolved.status, StatusCode::OK, "{}", resolved.body);
    assert_eq!(json_of(&resolved)["status"], "resolved");

    let unknown = client
        .api(
            "POST",
            "/api/v1/incidents",
            Some(&write),
            Some(json!({"title": "x", "message": "y", "monitors": ["ghost"]})),
        )
        .await;
    assert_eq!(unknown.status, StatusCode::UNPROCESSABLE_ENTITY);
    let missing = client
        .api(
            "POST",
            "/api/v1/incidents/999/updates",
            Some(&write),
            Some(json!({"status": "resolved"})),
        )
        .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn admins_issue_and_revoke_tokens_in_the_console() {
    let app = TestApp::new().await;
    let mut admin = app.signed_in("alice", 1001).await;

    admin
        .get("/admin/api")
        .await
        .assert_contains("No tokens yet.");
    let created = admin
        .post_form("/admin/api/tokens", &[("name", "ci"), ("scope", "write")])
        .await;
    assert_eq!(created.status, StatusCode::OK, "{}", created.body);
    assert_eq!(created.headers["cache-control"], "no-store");
    created.assert_contains("It is not shown again");
    let secret = created
        .body
        .split(r#"<code id="new-token">"#)
        .nth(1)
        .and_then(|rest| rest.split('<').next())
        .unwrap()
        .to_owned();
    assert!(secret.starts_with("upt_"));

    let mut client = app.client();
    let listed = client
        .api("GET", "/api/v1/monitors", Some(&secret), None)
        .await;
    assert_eq!(listed.status, StatusCode::OK);
    let stored = app.db.store().list_api_tokens().await.unwrap();
    assert_eq!(stored[0].name, "ci");
    admin.get("/admin/api").await.assert_contains("ci");

    let bad = admin
        .post_form("/admin/api/tokens", &[("name", " "), ("scope", "write")])
        .await;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);

    let revoked = admin
        .post_form(&format!("/admin/api/tokens/{}/delete", stored[0].id), &[])
        .await;
    assert_eq!(revoked.location(), Some("/admin/api"));
    let after = client
        .api("GET", "/api/v1/monitors", Some(&secret), None)
        .await;
    assert_eq!(after.status, StatusCode::UNAUTHORIZED);

    let stranger = app.client().get("/admin/api").await;
    assert_ne!(stranger.status, StatusCode::OK);
}

#[tokio::test]
async fn admin_mutations_reject_a_different_origin_on_the_same_site() {
    let app = TestApp::new().await;
    let mut admin = app.signed_in("alice", 1001).await;
    let forged = admin
        .post_form_from(
            "https://evil.example.com",
            "/admin/api/tokens",
            &[("name", "forged"), ("scope", "write")],
        )
        .await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN);
    assert!(app.db.store().list_api_tokens().await.unwrap().is_empty());

    let own = admin
        .post_form_from(
            "https://status.example.com",
            "/admin/api/tokens",
            &[("name", "own"), ("scope", "read")],
        )
        .await;
    assert_eq!(own.status, StatusCode::OK);
}

#[tokio::test]
async fn tcp_monitors_can_speak_and_use_tls() {
    let app = TestApp::new().await;
    let write = token(&app, TokenScope::Write).await;
    let mut client = app.client();

    let created = client
        .api(
            "PUT",
            "/api/v1/monitors/cache",
            Some(&write),
            Some(json!({
                "name": "Cache",
                "check": {
                    "type": "tcp", "host": "cache.example.com", "port": 6380,
                    "send": "PING\\r\\n", "expect": "+PONG", "tls": true
                }
            })),
        )
        .await;

    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    let body = json_of(&created);
    assert_eq!(body["check"]["expect"], "+PONG");
    assert_eq!(body["check"]["tls"], true);
    let stored = app.db.store().list_monitors().await.unwrap().remove(0);
    let uptime_domain::CheckSpec::Tcp(tcp) = stored.spec.check else {
        panic!("tcp")
    };
    assert_eq!(tcp.send_bytes(), b"PING\r\n");
}

#[tokio::test]
async fn monitors_can_be_grouped_and_filtered_by_group_prefix() {
    let app = TestApp::new().await;
    let write = token(&app, TokenScope::Write).await;
    let mut client = app.client();
    for (key, group) in [("a", "Prod/EU"), ("b", "prod"), ("c", "staging")] {
        let mut body = monitor(key);
        body["group"] = group.into();
        let reply = client
            .api(
                "PUT",
                &format!("/api/v1/monitors/{key}"),
                Some(&write),
                Some(body),
            )
            .await;
        assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
    }

    let prod = client
        .api("GET", "/api/v1/monitors?group=prod", Some(&write), None)
        .await;
    let keys: Vec<_> = json_of(&prod)["monitors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["key"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(keys, ["a", "b"]);
    assert_eq!(json_of(&prod)["monitors"][0]["group"], "prod/eu");

    let eu = client
        .api("GET", "/api/v1/monitors?group=prod/eu", Some(&write), None)
        .await;
    assert_eq!(json_of(&eu)["monitors"].as_array().unwrap().len(), 1);

    let bad = client
        .api(
            "PUT",
            "/api/v1/monitors/d",
            Some(&write),
            Some(json!({"name": "x", "group": "no way", "check": {"type": "tcp", "host": "h", "port": 1}})),
        )
        .await;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
}
