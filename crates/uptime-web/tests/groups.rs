#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! Group monitors in the console, the JSON API and on status pages.

mod common;

use std::time::Duration;

use axum::http::StatusCode;
use common::TestApp;
use jiff::Timestamp;
use pretty_assertions::assert_eq;
use serde_json::json;
use uptime_domain::{
    CheckPolicy, CheckSpec, ComponentSpec, GroupCheck, GroupMode, Health, MonitorId, MonitorSpec,
    MonitorState, PageSpec, Runtime, SectionSpec, TcpCheck, Theme, TokenScope, Verdict,
};
use uptime_store::{CheckRecord, NewApiToken};

fn tcp(key: &str, name: &str, folder: &str) -> MonitorSpec {
    MonitorSpec {
        key: key.parse().unwrap(),
        name: name.into(),
        check: CheckSpec::Tcp(TcpCheck::connect(format!("{key}.example.com"), 5432)),
        policy: CheckPolicy::default(),
        active: true,
        tags: Vec::new(),
        group: Some(folder.into()),
    }
}

fn group(key: &str, path: &str, show_members: bool) -> MonitorSpec {
    MonitorSpec {
        key: key.parse().unwrap(),
        name: "Platform".into(),
        check: CheckSpec::Group(GroupCheck {
            path: path.into(),
            mode: GroupMode::Any,
            show_members,
        }),
        policy: CheckPolicy::default(),
        active: true,
        tags: Vec::new(),
        group: None,
    }
}

async fn record(app: &TestApp, id: MonitorId, state: MonitorState) {
    let now = Timestamp::now();
    app.db
        .store()
        .record_check(&CheckRecord {
            monitor_id: id,
            scheduled_for: now,
            checked_at: now,
            verdict: Verdict {
                health: if state == MonitorState::Down {
                    Health::Down
                } else {
                    Health::Up
                },
                latency: Some(Duration::from_millis(20)),
                status_code: None,
                reason: None,
                response_body: None,
            },
            runtime: Runtime {
                state,
                consecutive_failures: 0,
            },
            transition: None,
            cert: None,
            next_run_at: now,
            region: "test".into(),
        })
        .await
        .unwrap();
}

/// `api` (up) and `db` (down) in `prod`, plus `web` in `dev`.
async fn members(app: &TestApp) {
    let store = app.db.store();
    for (key, name, folder, state) in [
        ("api", "Public API", "prod", MonitorState::Up),
        ("db", "Orders database", "prod/eu", MonitorState::Down),
        ("web", "Website", "dev", MonitorState::Up),
    ] {
        let monitor = store
            .create_monitor(&tcp(key, name, folder), Timestamp::now())
            .await
            .unwrap();
        record(app, monitor.id, state).await;
    }
}

fn form(key: &str, path: &str) -> Vec<(&'static str, String)> {
    vec![
        ("key", key.to_owned()),
        ("name", "Platform".to_owned()),
        ("check_type", "group".to_owned()),
        ("group_path", path.to_owned()),
        ("group_mode", "all".to_owned()),
        ("group_show_members", "on".to_owned()),
        ("interval", "1m".to_owned()),
        ("retry_interval", "20s".to_owned()),
        ("timeout", "5s".to_owned()),
        ("retries", "0".to_owned()),
        ("active", "on".to_owned()),
    ]
}

fn fields<'a>(pairs: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    pairs.iter().map(|(k, v)| (*k, v.as_str())).collect()
}

#[tokio::test]
async fn group_monitors_are_created_in_the_console_and_list_their_members() {
    let app = TestApp::new().await;
    members(&app).await;
    let mut client = app.signed_in("alice", 1001).await;

    let new = client.get("/admin/monitors/new").await;
    new.assert_contains(r#"<option value="group""#)
        .assert_contains(r#"<option value="prod/eu">"#);

    let reply = client
        .post_form("/admin/monitors", &fields(&form("platform", " Prod ")))
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    let monitor = app
        .db
        .store()
        .list_monitors()
        .await
        .unwrap()
        .into_iter()
        .find(|m| m.spec.key.as_str() == "platform")
        .unwrap();
    assert_eq!(
        monitor.spec.check,
        CheckSpec::Group(GroupCheck {
            path: "prod".into(),
            mode: GroupMode::All,
            show_members: true,
        })
    );

    let detail = client.get(reply.location().unwrap()).await;
    detail
        .assert_contains("Members")
        .assert_contains("Public API")
        .assert_contains("Orders database")
        .assert_contains("every member is down");
    assert!(!detail.body.contains("Website"), "not in prod");

    // The edit form round-trips the settings.
    let edit = client
        .get(&format!("/admin/monitors/{}/edit", monitor.id))
        .await;
    edit.assert_contains(r#"name="group_path" value="prod""#)
        .assert_contains(r#"<option value="all" selected"#);

    // The dashboard's folder header points at the group monitor.
    client
        .get("/admin")
        .await
        .assert_contains("group prod, all")
        .assert_contains(&format!(r#"href="/admin/monitors/{}""#, monitor.id));

    // "Test now" judges the members as they are.
    let tested = client
        .datastar_signals(&format!("/admin/monitors/{}/test", monitor.id), &json!({}))
        .await;
    tested.assert_contains("Degraded");
}

#[tokio::test]
async fn group_forms_explain_bad_paths_and_cycles() {
    let app = TestApp::new().await;
    let mut client = app.signed_in("alice", 1001).await;
    let bad = client
        .post_form("/admin/monitors", &fields(&form("platform", "no way")))
        .await;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
    bad.assert_contains("group path such as");

    app.db
        .store()
        .create_monitor(
            &MonitorSpec {
                group: Some("x".into()),
                ..group("a", "y", false)
            },
            Timestamp::now(),
        )
        .await
        .unwrap();
    let mut cycle = form("b", "x");
    cycle.push(("group", "y".to_owned()));
    let reply = client.post_form("/admin/monitors", &fields(&cycle)).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    reply.assert_contains("would include each other: a, b");
}

#[tokio::test]
async fn group_monitors_work_through_the_api() {
    let app = TestApp::new().await;
    let secret = uptime_web::new_api_token();
    app.db
        .store()
        .create_api_token(
            &NewApiToken {
                name: "test".into(),
                token_hash: uptime_web::hash_api_token(&secret),
                scope: TokenScope::Write,
                created_by: "alice".into(),
            },
            Timestamp::now(),
        )
        .await
        .unwrap();
    let mut client = app.client();

    let created = client
        .api(
            "PUT",
            "/api/v1/monitors/platform",
            Some(&secret),
            Some(json!({
                "name": "Platform",
                "group": "x",
                "check": {"type": "group", "path": "y", "mode": "all"},
            })),
        )
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    created.assert_contains(r#""type":"group""#);

    let cycle = client
        .api(
            "PUT",
            "/api/v1/monitors/back",
            Some(&secret),
            Some(json!({
                "name": "Back",
                "group": "y",
                "check": {"type": "group", "path": "x"},
            })),
        )
        .await;
    assert_eq!(cycle.status, StatusCode::UNPROCESSABLE_ENTITY);
    cycle.assert_contains("would include each other");

    let bad = client
        .api(
            "PUT",
            "/api/v1/monitors/bad",
            Some(&secret),
            Some(json!({"name": "Bad", "check": {"type": "group", "path": "No Way"}})),
        )
        .await;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
}

fn page(slug: &str, key: &str) -> PageSpec {
    PageSpec {
        slug: slug.parse().unwrap(),
        title: "Platform Status".into(),
        description: None,
        accent: None,
        theme: Theme::Auto,
        look: Default::default(),
        published: true,
        website: None,
        custom_css: None,
        sections: vec![SectionSpec {
            name: "Core".into(),
            components: vec![ComponentSpec::of_monitor(key.parse().unwrap(), None)],
        }],
    }
}

#[tokio::test]
async fn status_pages_show_a_group_and_optionally_its_members() {
    let app = TestApp::new().await;
    members(&app).await;
    let store = app.db.store();
    let shown = store
        .create_monitor(&group("platform", "prod", true), Timestamp::now())
        .await
        .unwrap();
    record(&app, shown.id, MonitorState::Down).await;
    store
        .create_page(&page("platform", "platform"))
        .await
        .unwrap();

    let reply = app.client().get("/s/platform").await;
    reply
        .assert_contains("Platform")
        .assert_contains("component-members")
        .assert_contains("Public API")
        .assert_contains("Orders database");
    assert!(!reply.body.contains("Website"));
    assert!(!reply.body.contains("api.example.com"), "no targets");

    // Without `show_members`, only the group appears.
    store
        .create_monitor(&group("quiet", "prod", false), Timestamp::now())
        .await
        .unwrap();
    store.create_page(&page("quiet", "quiet")).await.unwrap();
    let reply = app.client().get("/s/quiet").await;
    reply.assert_contains("Platform");
    assert!(!reply.body.contains("Orders database"));
}
