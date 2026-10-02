#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! The admin console over HTTP, signed in through the real OAuth flow.

mod common;

use std::time::Duration;

use axum::http::StatusCode;
use common::{TestApp, read_stream_until};
use jiff::Timestamp;
use pretty_assertions::assert_eq;
use uptime_domain::{
    CheckPolicy, CheckSpec, Health, MonitorId, MonitorSpec, MonitorState, Runtime, TcpCheck,
    Verdict,
};
use uptime_runtime::CheckCompleted;
use uptime_store::CheckRecord;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::any};

fn tcp(key: &str) -> MonitorSpec {
    MonitorSpec {
        key: key.parse().unwrap(),
        name: format!("{key} database"),
        check: CheckSpec::Tcp(TcpCheck::connect(format!("{key}.example.com"), 5432)),
        policy: CheckPolicy::default(),
        active: true,
        tags: Vec::new(),
        group: None,
    }
}

fn form(key: &str, url: &str) -> Vec<(&'static str, String)> {
    vec![
        ("key", key.to_owned()),
        ("name", "Public API".to_owned()),
        ("check_type", "http".to_owned()),
        ("url", url.to_owned()),
        ("method", "GET".to_owned()),
        ("accepted_status", "200-299".to_owned()),
        ("interval", "1m".to_owned()),
        ("retry_interval", "20s".to_owned()),
        ("timeout", "5s".to_owned()),
        ("retries", "1".to_owned()),
        ("active", "on".to_owned()),
    ]
}

fn fields<'a>(pairs: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    pairs.iter().map(|(k, v)| (*k, v.as_str())).collect()
}

fn id_from_location(location: &str) -> MonitorId {
    MonitorId(
        location
            .trim_start_matches("/admin/monitors/")
            .parse()
            .unwrap(),
    )
}

#[tokio::test]
async fn the_dashboard_lists_monitors_with_their_state() {
    let app = TestApp::new().await;
    app.db
        .store()
        .create_monitor(&tcp("orders"), Timestamp::now())
        .await
        .unwrap();
    let mut client = app.signed_in("alice", 1001).await;

    let reply = client.get("/admin").await;

    assert_eq!(reply.status, StatusCode::OK);
    reply
        .assert_contains("orders database")
        .assert_contains("orders.example.com:5432")
        .assert_contains("Unknown")
        .assert_contains(r#"data-init="@get('/admin/live')""#);
}

#[tokio::test]
async fn an_empty_dashboard_says_how_to_start() {
    let app = TestApp::new().await;
    let reply = app.signed_in("alice", 1001).await.get("/admin").await;
    reply.assert_contains("No monitors yet");
}

#[tokio::test]
async fn creating_a_monitor_saves_it_and_shows_it() {
    let app = TestApp::new().await;
    let mut client = app.signed_in("alice", 1001).await;
    assert_eq!(
        client.get("/admin/monitors/new").await.status,
        StatusCode::OK
    );

    let reply = client
        .post_form(
            "/admin/monitors",
            &fields(&form("public-api", "https://api.example.com/health")),
        )
        .await;

    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    let id = id_from_location(reply.location().unwrap());
    let monitor = app.db.store().monitor(id).await.unwrap().unwrap();
    assert_eq!(monitor.spec.name, "Public API");
    assert_eq!(monitor.spec.policy.retries, 1);
    client
        .get(reply.location().unwrap())
        .await
        .assert_contains("Public API")
        .assert_contains("api.example.com");
}

#[tokio::test]
async fn invalid_input_is_shown_back_with_messages() {
    let app = TestApp::new().await;
    let mut client = app.signed_in("alice", 1001).await;
    let mut input = form("Bad Key", "not a url");
    input.push(("name", String::new()));
    input.retain(|(k, v)| *k != "name" || v.is_empty());

    let reply = client.post_form("/admin/monitors", &fields(&input)).await;

    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    reply
        .assert_contains("lowercase letters, digits and dashes")
        .assert_contains("http:// or https:// URL")
        .assert_contains("Give the monitor a name")
        .assert_contains(r#"value="Bad Key""#);
    assert!(app.db.store().list_monitors().await.unwrap().is_empty());
}

#[tokio::test]
async fn duplicate_keys_are_explained() {
    let app = TestApp::new().await;
    app.db
        .store()
        .create_monitor(&tcp("taken"), Timestamp::now())
        .await
        .unwrap();
    let mut client = app.signed_in("alice", 1001).await;

    let reply = client
        .post_form(
            "/admin/monitors",
            &fields(&form("taken", "https://x.example.com")),
        )
        .await;

    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    reply.assert_contains("already uses the key");
}

#[tokio::test]
async fn editing_a_monitor_updates_it() {
    let app = TestApp::new().await;
    let monitor = app
        .db
        .store()
        .create_monitor(&tcp("orders"), Timestamp::now())
        .await
        .unwrap();
    let mut client = app.signed_in("alice", 1001).await;
    let edit_page = client
        .get(&format!("/admin/monitors/{}/edit", monitor.id))
        .await;
    edit_page.assert_contains(r#"value="orders.example.com""#);

    let reply = client
        .post_form(
            &format!("/admin/monitors/{}", monitor.id),
            &fields(&form("orders", "https://orders.example.com")),
        )
        .await;

    assert_eq!(
        reply.location(),
        Some(format!("/admin/monitors/{}", monitor.id).as_str())
    );
    let updated = app.db.store().monitor(monitor.id).await.unwrap().unwrap();
    assert!(matches!(updated.spec.check, CheckSpec::Http(_)));
}

#[tokio::test]
async fn pausing_resuming_and_deleting() {
    let app = TestApp::new().await;
    let monitor = app
        .db
        .store()
        .create_monitor(&tcp("orders"), Timestamp::now())
        .await
        .unwrap();
    let mut client = app.signed_in("alice", 1001).await;
    let base = format!("/admin/monitors/{}", monitor.id);

    client.post_form(&format!("{base}/pause"), &[]).await;
    assert_eq!(
        app.db
            .store()
            .runtime(monitor.id)
            .await
            .unwrap()
            .unwrap()
            .runtime
            .state,
        MonitorState::Paused
    );

    client.post_form(&format!("{base}/resume"), &[]).await;
    assert_eq!(
        app.db
            .store()
            .runtime(monitor.id)
            .await
            .unwrap()
            .unwrap()
            .runtime
            .state,
        MonitorState::Unknown
    );

    let reply = client.post_form(&format!("{base}/delete"), &[]).await;
    assert_eq!(reply.location(), Some("/admin"));
    assert_eq!(app.db.store().monitor(monitor.id).await.unwrap(), None);
}

#[tokio::test]
async fn unknown_monitors_are_not_found() {
    let app = TestApp::new().await;
    let mut client = app.signed_in("alice", 1001).await;
    assert_eq!(
        client.get("/admin/monitors/424242").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        client.get("/admin/monitors/not-a-number").await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn the_detail_page_shows_recent_checks() {
    let app = TestApp::new().await;
    let monitor = app
        .db
        .store()
        .create_monitor(&tcp("orders"), Timestamp::now())
        .await
        .unwrap();
    let now = Timestamp::now();
    app.db
        .store()
        .record_check(&CheckRecord {
            monitor_id: monitor.id,
            scheduled_for: now,
            checked_at: now,
            verdict: Verdict {
                health: Health::Up,
                latency: Some(Duration::from_millis(87)),
                status_code: None,
                reason: None,
                response_body: None,
            },
            runtime: Runtime {
                state: MonitorState::Up,
                consecutive_failures: 0,
            },
            transition: None,
            cert: None,
            next_run_at: now,
            region: "arn".into(),
        })
        .await
        .unwrap();
    let mut client = app.signed_in("alice", 1001).await;

    let reply = client.get(&format!("/admin/monitors/{}", monitor.id)).await;

    reply
        .assert_contains("87ms")
        .assert_contains("arn")
        .assert_contains("Recent checks")
        .assert_contains("Response time")
        .assert_contains(r#"class="chart""#)
        .assert_contains("median 87ms")
        .assert_contains("24 hours")
        .assert_contains("90 days");

    let week = client
        .get(&format!("/admin/monitors/{}?range=7d", monitor.id))
        .await;
    week.assert_contains(r#"aria-current="true">7d"#);
}

#[tokio::test]
async fn test_now_probes_unsaved_input() {
    let app = TestApp::new().await;
    let target = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(418))
        .mount(&target)
        .await;
    let mut client = app.signed_in("alice", 1001).await;

    let reply = client
        .datastar_form("/admin/monitors/test", &fields(&form("try", &target.uri())))
        .await;

    assert_eq!(reply.status, StatusCode::OK);
    assert!(
        reply.headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    reply
        .assert_contains("datastar-patch-elements")
        .assert_contains("test-result")
        .assert_contains("unexpected status 418");
}

#[tokio::test]
async fn test_now_reports_form_errors() {
    let app = TestApp::new().await;
    let mut client = app.signed_in("alice", 1001).await;
    let reply = client
        .datastar_form("/admin/monitors/test", &fields(&form("try", "nope")))
        .await;
    reply
        .assert_contains("test-result")
        .assert_contains("Fix the highlighted fields");
}

#[tokio::test]
async fn the_live_stream_patches_rows_after_checks() {
    let app = TestApp::new().await;
    let monitor = app
        .db
        .store()
        .create_monitor(&tcp("orders"), Timestamp::now())
        .await
        .unwrap();
    let mut client = app.signed_in("alice", 1001).await;
    let stream = client.open_stream("/admin/live").await;
    assert_eq!(stream.status(), StatusCode::OK);

    let events = app.state.events.clone();
    let spec_key = monitor.spec.key.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        events.publish(CheckCompleted {
            monitor_id: monitor.id,
            key: spec_key,
            previous: MonitorState::Unknown,
            state: MonitorState::Down,
            transition: None,
            latency_ms: None,
            checked_at: Timestamp::now(),
        });
    });

    let seen = read_stream_until(stream, &format!("monitor-{}", monitor.id)).await;
    assert!(seen.contains("datastar-patch-elements"), "{seen}");
}

#[tokio::test]
async fn the_monitor_page_updates_live_after_checks() {
    let app = TestApp::new().await;
    let monitor = app
        .db
        .store()
        .create_monitor(&tcp("orders"), Timestamp::now())
        .await
        .unwrap();
    let mut client = app.signed_in("alice", 1001).await;
    client
        .get(&format!("/admin/monitors/{}", monitor.id))
        .await
        .assert_contains(&format!("@get('/admin/monitors/{}/live')", monitor.id));
    let stream = client
        .open_stream(&format!("/admin/monitors/{}/live", monitor.id))
        .await;
    assert_eq!(stream.status(), StatusCode::OK);

    let events = app.state.events.clone();
    let key = monitor.spec.key.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        events.publish(CheckCompleted {
            monitor_id: monitor.id,
            key,
            previous: MonitorState::Unknown,
            state: MonitorState::Down,
            transition: None,
            latency_ms: None,
            checked_at: Timestamp::now(),
        });
    });

    let seen = read_stream_until(stream, "recent-checks").await;
    assert!(
        seen.contains("monitor-now") && seen.contains("monitor-state"),
        "{seen}"
    );
}

#[tokio::test]
async fn the_live_stream_requires_signing_in() {
    let app = TestApp::new().await;
    let response = app.client().open_stream("/admin/live").await;
    assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
}

/// The first `attr="…"` value in `html` that starts with `prefix`.
fn find_url<'a>(html: &'a str, prefix: &str) -> &'a str {
    let start = html
        .find(prefix)
        .unwrap_or_else(|| panic!("{prefix} not in page"));
    let end = start + html[start..].find('"').unwrap();
    &html[start..end]
}

#[tokio::test]
async fn versioned_assets_are_cached_for_a_year() {
    let app = TestApp::new().await;
    let mut client = app.client();
    let page = client.get("/login").await;
    let css_url = find_url(&page.body, "/static/app.css?v=");
    let js_url = find_url(&page.body, "/static/datastar.js?v=");

    let css = client.get(css_url).await;
    let js = client.get(js_url).await;

    assert_eq!(css.status, StatusCode::OK);
    assert!(
        css.headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/css")
    );
    assert!(
        css.headers["cache-control"]
            .to_str()
            .unwrap()
            .contains("immutable")
    );
    css.assert_contains("--sb-brand");
    assert_eq!(js.status, StatusCode::OK);
    js.assert_contains("Datastar v1.0.4 + Rocket");
}

#[tokio::test]
async fn unversioned_assets_are_cached_briefly() {
    let app = TestApp::new().await;
    let reply = app.client().get("/static/app.css").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.headers["cache-control"], "public, max-age=3600");
}

#[tokio::test]
async fn starbase_components_resolve_datastar_through_the_import_map() {
    let app = TestApp::new().await;
    let mut client = app.client();
    let page = app.signed_in("alice", 1001).await.get("/admin").await;
    let datastar = find_url(&page.body, "/static/datastar.js?v=");

    page.assert_contains(&format!(
        r#"<script type="importmap">{{"imports":{{"datastar":"{datastar}"}}}}</script>"#
    ));
    let importmap_at = page.body.find("importmap").unwrap();
    let first_module = page.body.find(r#"type="module""#).unwrap();
    assert!(
        importmap_at < first_module,
        "the import map must precede module scripts"
    );

    let toggle = client
        .get(find_url(
            &page.body,
            "/static/starbase/c/toggle/toggle.min.js?v=",
        ))
        .await;
    assert_eq!(toggle.status, StatusCode::OK);
    assert!(
        toggle.headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/javascript")
    );
    toggle.assert_contains(r#"from"datastar""#);

    let font = client
        .get("/static/starbase/fonts/pixelify-sans.woff2")
        .await;
    assert_eq!(font.status, StatusCode::OK);
    assert_eq!(font.headers["content-type"], "font/woff2");
    assert_eq!(
        client.get("/static/starbase/LICENSE").await.status,
        StatusCode::OK
    );
    assert_eq!(
        client.get("/static/nope.js").await.status,
        StatusCode::NOT_FOUND
    );
}

async fn add_grouped(app: &TestApp, key: &str, group: Option<&str>) -> MonitorId {
    let spec = MonitorSpec {
        group: group.map(str::to_owned),
        ..tcp(key)
    };
    app.db
        .store()
        .create_monitor(&spec, Timestamp::now())
        .await
        .unwrap()
        .id
}

#[tokio::test]
async fn the_dashboard_shows_groups_as_a_tree_with_their_worst_state() {
    let app = TestApp::new().await;
    add_grouped(&app, "loose", None).await;
    add_grouped(&app, "web-eu", Some("prod/eu")).await;
    let down = add_grouped(&app, "db-eu", Some("prod/eu")).await;
    add_grouped(&app, "web-us", Some("prod")).await;
    let now = Timestamp::now();
    app.db
        .store()
        .record_check(&CheckRecord {
            monitor_id: down,
            scheduled_for: now,
            checked_at: now,
            verdict: Verdict {
                health: Health::Down,
                latency: None,
                status_code: None,
                reason: None,
                response_body: None,
            },
            runtime: Runtime {
                state: MonitorState::Down,
                consecutive_failures: 3,
            },
            transition: None,
            cert: None,
            next_run_at: now,
            region: "arn".into(),
        })
        .await
        .unwrap();
    let mut client = app.signed_in("alice", 1001).await;

    let reply = client.get("/admin").await;

    reply
        .assert_contains(r#"data-group="prod""#)
        .assert_contains(r#"data-group="prod/eu""#)
        .assert_contains("3 monitors")
        .assert_contains("2 monitors");
    // prod (depth 0) → its own monitor and prod/eu (depth 1) → its monitors.
    let body = &reply.body;
    let at = |needle: &str| body.find(needle).unwrap_or_else(|| panic!("{needle}"));
    assert!(
        at("monitor-1") < at(r#"data-group="prod""#),
        "ungrouped first"
    );
    assert!(at(r#"data-group="prod""#) < at(r#"data-group="prod/eu""#));
    assert!(at(r#"data-group="prod/eu""#) < at("monitor-2"));
    // The group with a down child shows down.
    let eu = &body[at(r#"data-group="prod/eu""#)..];
    assert!(eu[..eu.find("</tr>").unwrap()].contains("pill-down"));
}

async fn add_monitors(app: &TestApp, keys: &[(&str, &[&str])]) -> Vec<MonitorId> {
    let mut ids = Vec::new();
    for (key, tags) in keys {
        let spec = MonitorSpec {
            tags: tags.iter().map(|t| (*t).to_owned()).collect(),
            ..tcp(key)
        };
        ids.push(
            app.db
                .store()
                .create_monitor(&spec, Timestamp::now())
                .await
                .unwrap()
                .id,
        );
    }
    ids
}

#[tokio::test]
async fn the_dashboard_offers_search_tag_filters_and_selection() {
    let app = TestApp::new().await;
    add_monitors(&app, &[("orders", &["db", "prod"]), ("billing", &["prod"])]).await;
    let mut client = app.signed_in("alice", 1001).await;

    let reply = client.get("/admin").await;

    reply
        .assert_contains(r#"data-bind:q"#)
        .assert_contains(r#"data-tags="db prod""#)
        .assert_contains("$tagf = $tagf == 'db' ? '' : 'db'")
        .assert_contains("Add tag")
        // Rows toggle `$sel` explicitly: `data-bind` on a checkbox group filled
        // the signal with every row's value on load.
        .assert_contains("$sel = evt.target.checked ? [...$sel, ")
        .assert_contains("el.checked = $sel.includes(");
    assert!(!reply.body.contains("data-bind:sel"));
}

#[tokio::test]
async fn tags_are_saved_from_the_monitor_form() {
    let app = TestApp::new().await;
    let mut client = app.signed_in("alice", 1001).await;
    let mut pairs = form("tagged", "https://example.com");
    pairs.push(("tags", "Prod, api".to_owned()));

    let reply = client.post_form("/admin/monitors", &fields(&pairs)).await;

    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    let monitor = app.db.store().list_monitors().await.unwrap().remove(0);
    assert_eq!(monitor.spec.tags, ["api", "prod"]);
}

#[tokio::test]
async fn bulk_actions_pause_tag_untag_and_delete_the_selection() {
    let app = TestApp::new().await;
    let ids = add_monitors(&app, &[("a", &["prod"]), ("b", &[]), ("c", &[])]).await;
    let mut client = app.signed_in("alice", 1001).await;
    let store = app.db.store();
    let pick = |ids: &[MonitorId]| ids.iter().map(|id| id.0.to_string()).collect::<Vec<_>>();
    let send = |action: &str, sel: Vec<String>, tag: &str| serde_json::json!({"sel": sel, "bulk_action": action, "bulk_tag": tag, "q": ""});

    let paused = client
        .datastar_signals("/admin/monitors/bulk", &send("pause", pick(&ids[..2]), ""))
        .await;
    assert_eq!(paused.status, StatusCode::OK, "{}", paused.body);
    paused.assert_contains("location.reload()");
    assert!(!store.monitor(ids[0]).await.unwrap().unwrap().spec.active);
    assert!(!store.monitor(ids[1]).await.unwrap().unwrap().spec.active);
    assert!(store.monitor(ids[2]).await.unwrap().unwrap().spec.active);

    client
        .datastar_signals("/admin/monitors/bulk", &send("resume", pick(&ids[..1]), ""))
        .await;
    assert!(store.monitor(ids[0]).await.unwrap().unwrap().spec.active);

    client
        .datastar_signals(
            "/admin/monitors/bulk",
            &send("tag", pick(&ids), "Team:Core, eu"),
        )
        .await;
    assert_eq!(
        store.monitor(ids[0]).await.unwrap().unwrap().spec.tags,
        ["eu", "prod", "team:core"]
    );
    assert_eq!(
        store.monitor(ids[1]).await.unwrap().unwrap().spec.tags,
        ["eu", "team:core"]
    );

    client
        .datastar_signals("/admin/monitors/bulk", &send("untag", pick(&ids), "eu"))
        .await;
    assert_eq!(
        store.monitor(ids[1]).await.unwrap().unwrap().spec.tags,
        ["team:core"]
    );

    client
        .datastar_signals("/admin/monitors/bulk", &send("delete", pick(&ids[1..]), ""))
        .await;
    assert_eq!(store.list_monitors().await.unwrap().len(), 1);
}

#[tokio::test]
async fn bulk_actions_explain_what_is_missing_and_need_sign_in() {
    let app = TestApp::new().await;
    let ids = add_monitors(&app, &[("a", &[])]).await;
    let mut client = app.signed_in("alice", 1001).await;

    let none = client
        .datastar_signals(
            "/admin/monitors/bulk",
            &serde_json::json!({"sel": [], "bulk_action": "pause"}),
        )
        .await;
    none.assert_contains("Select at least one monitor.");
    let untagged = client
        .datastar_signals(
            "/admin/monitors/bulk",
            &serde_json::json!({"sel": [ids[0].0.to_string()], "bulk_action": "tag", "bulk_tag": ""}),
        )
        .await;
    untagged.assert_contains("Enter a tag.");
    let bad = client
        .datastar_signals(
            "/admin/monitors/bulk",
            &serde_json::json!({"sel": [ids[0].0.to_string()], "bulk_action": "tag", "bulk_tag": "no way!"}),
        )
        .await;
    bad.assert_contains("tags may use");

    let mut stranger = app.client();
    let stranger = stranger
        .datastar_signals(
            "/admin/monitors/bulk",
            &serde_json::json!({"sel": [ids[0].0.to_string()], "bulk_action": "delete"}),
        )
        .await;
    assert_ne!(stranger.status, StatusCode::OK);
    assert_eq!(app.db.store().list_monitors().await.unwrap().len(), 1);
}
