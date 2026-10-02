#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! Public status pages and the admin page editor.

mod common;

use std::time::Duration;

use axum::http::StatusCode;
use common::TestApp;
use jiff::Timestamp;
use pretty_assertions::assert_eq;
use uptime_domain::{
    CheckPolicy, CheckSpec, ComponentSpec, Health, HttpCheck, MonitorId, MonitorSpec, MonitorState,
    PageSpec, Runtime, SectionSpec, Theme, Verdict,
};
use uptime_store::CheckRecord;

fn monitor(key: &str, name: &str) -> MonitorSpec {
    MonitorSpec {
        key: key.parse().unwrap(),
        name: name.into(),
        check: CheckSpec::Http(HttpCheck::get(
            format!("https://secret-{key}.internal.example/health")
                .parse()
                .unwrap(),
        )),
        policy: CheckPolicy::default(),
        active: true,
        tags: Vec::new(),
        group: None,
    }
}

fn page(slug: &str, published: bool) -> PageSpec {
    PageSpec {
        slug: slug.parse().unwrap(),
        title: "Platform Status".into(),
        description: Some("Core services for the platform".into()),
        accent: Some("#4f46e5".parse().unwrap()),
        theme: Theme::Auto,
        look: Default::default(),
        published,
        website: None,
        sections: vec![SectionSpec {
            name: "Core".into(),
            components: vec![
                ComponentSpec {
                    monitor: "api".parse().unwrap(),
                    label: Some("Public API".into()),
                },
                ComponentSpec {
                    monitor: "web".parse().unwrap(),
                    label: None,
                },
            ],
        }],
    }
}

async fn record(app: &TestApp, id: MonitorId, state: MonitorState) {
    let now = Timestamp::now();
    let health = if state == MonitorState::Down {
        Health::Down
    } else {
        Health::Up
    };
    app.db
        .store()
        .record_check(&CheckRecord {
            monitor_id: id,
            scheduled_for: now,
            checked_at: now,
            verdict: Verdict {
                health,
                latency: Some(Duration::from_millis(20)),
                status_code: Some(200),
                reason: None,
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

/// Two monitors (api up, web in `web_state`) on a page `platform`.
async fn setup(web_state: MonitorState, published: bool) -> TestApp {
    let app = TestApp::new().await;
    let store = app.db.store();
    let api = store
        .create_monitor(&monitor("api", "API service"), Timestamp::now())
        .await
        .unwrap();
    let web = store
        .create_monitor(&monitor("web", "Web app"), Timestamp::now())
        .await
        .unwrap();
    record(&app, api.id, MonitorState::Up).await;
    record(&app, web.id, web_state).await;
    store
        .create_page(&page("platform", published))
        .await
        .unwrap();
    app
}

#[tokio::test]
async fn a_healthy_page_says_all_systems_operational() {
    let app = setup(MonitorState::Up, true).await;

    let reply = app.client().get("/s/platform").await;

    assert_eq!(reply.status, StatusCode::OK);
    reply
        .assert_contains("<title>Platform Status · uptimestatus</title>")
        .assert_contains("All systems operational")
        .assert_contains("Public API")
        .assert_contains("Web app")
        .assert_contains("Core services for the platform")
        .assert_contains("100.00% uptime")
        .assert_contains("--accent: #4f46e5");
    assert_eq!(
        reply.body.matches(" data-day=").count(),
        180,
        "90 days × 2 components"
    );
    assert!(
        !reply.body.contains("secret-"),
        "targets must never leak to the public page"
    );
    assert!(
        reply.headers["cache-control"]
            .to_str()
            .unwrap()
            .contains("max-age=10")
    );
}

#[tokio::test]
async fn an_outage_is_the_headline() {
    let app = setup(MonitorState::Down, true).await;
    let reply = app.client().get("/s/platform").await;
    reply
        .assert_contains("Partial outage")
        .assert_contains("pill-down");
}

#[tokio::test]
async fn unknown_or_unpublished_pages_are_not_found() {
    let app = setup(MonitorState::Up, false).await;
    assert_eq!(
        app.client().get("/s/platform").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.client().get("/s/nope").await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn admins_can_preview_unpublished_pages() {
    let app = setup(MonitorState::Up, false).await;
    let reply = app.signed_in("alice", 1001).await.get("/s/platform").await;
    assert_eq!(reply.status, StatusCode::OK);
    reply.assert_contains("Draft");
}

#[tokio::test]
async fn the_page_updates_live_when_its_monitors_are_checked() {
    let app = setup(MonitorState::Up, true).await;
    let mut client = app.client();
    client
        .get("/s/platform")
        .await
        .assert_contains("@get('/s/platform/live')");
    let stream = client.open_stream("/s/platform/live").await;
    assert!(
        stream.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    let web = app
        .db
        .store()
        .list_monitors()
        .await
        .unwrap()
        .into_iter()
        .find(|m| m.spec.key.as_str() == "web")
        .unwrap();
    record(&app, web.id, MonitorState::Down).await;
    let events = app.state.events.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        events.publish(uptime_runtime::CheckCompleted {
            monitor_id: web.id,
            key: web.spec.key.clone(),
            previous: MonitorState::Up,
            state: MonitorState::Down,
            transition: None,
            latency_ms: None,
            checked_at: Timestamp::now(),
        });
    });

    let seen = common::read_stream_until(stream, "Partial outage").await;

    assert!(
        seen.contains("datastar-patch-elements") && seen.contains("status-body"),
        "{seen}"
    );
}

#[tokio::test]
async fn the_body_can_still_be_fetched_on_its_own() {
    let app = setup(MonitorState::Up, true).await;
    let body = app.client().get("/s/platform/body").await;
    body.assert_contains("datastar-patch-elements")
        .assert_contains("status-body");
}

#[tokio::test]
async fn times_show_the_exact_moment_on_hover() {
    let app = setup(MonitorState::Up, true).await;
    app.client()
        .get("/s/platform")
        .await
        .assert_contains(r#"<time class="ago" datetime=""#)
        .assert_contains(" UTC\"")
        .assert_contains("data-attr:title=\"new Date(");
}

#[tokio::test]
async fn bars_describe_their_day_in_one_shared_hover_card() {
    let app = setup(MonitorState::Up, true).await;
    let today = Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC).date();

    let reply = app.client().get("/s/platform").await;

    reply
        .assert_contains(&format!(r#"data-day="{today}""#))
        .assert_contains(r#"data-label="Operational""#)
        .assert_contains(r#"data-uptime="100.00% uptime""#)
        .assert_contains(r#"data-label="No data""#)
        .assert_contains("data-on:pointerover");
    assert_eq!(reply.body.matches(r#"class="bar-tip""#).count(), 1);
    assert!(
        !reply.body.contains(&format!(r#"title="{today}:"#)),
        "no native tooltips on bars"
    );
}

#[tokio::test]
async fn summary_json_describes_the_page() {
    let app = setup(MonitorState::Down, true).await;

    let reply = app.client().get("/s/platform/summary.json").await;

    assert_eq!(reply.status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
    assert_eq!(json["page"]["slug"], "platform");
    assert_eq!(json["status"], "partial_outage");
    assert_eq!(json["components"][0]["name"], "Public API");
    assert_eq!(json["components"][0]["status"], "up");
    assert_eq!(json["components"][1]["status"], "down");
    assert!(!reply.body.contains("secret-"));
}

#[tokio::test]
async fn custom_domains_serve_their_page_at_the_root() {
    let app = setup(MonitorState::Up, true).await;
    app.state
        .domains
        .replace([("status.team.dev".to_owned(), "platform".to_owned())]);

    let mut client = app.client();
    let reply = client.get_on("status.team.dev", "/").await;

    assert_eq!(reply.status, StatusCode::OK);
    reply.assert_contains("All systems operational");
}

// ── Look ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_pixel_look_is_the_default_and_keeps_the_starfield() {
    let app = setup(MonitorState::Up, true).await;

    let reply = app.client().get("/s/platform").await;

    reply.assert_contains("<sb-starfield");
    assert!(!reply.body.contains("data-look"), "{}", reply.body);
}

#[tokio::test]
async fn the_clean_look_drops_the_backdrop_and_marks_the_page() {
    let app = setup(MonitorState::Up, true).await;
    let id = app.db.store().list_pages().await.unwrap()[0].id;
    let mut spec = app.db.store().page(id).await.unwrap().unwrap().spec;
    spec.look = uptime_domain::Look::Clean;
    app.db.store().update_page(id, &spec).await.unwrap();

    let reply = app.client().get("/s/platform").await;

    assert_eq!(reply.status, StatusCode::OK);
    reply
        .assert_contains(r#"data-look="clean""#)
        .assert_contains("All systems operational");
    assert!(!reply.body.contains("<sb-starfield"), "{}", reply.body);
    assert!(!reply.body.contains("starfield.min.js"), "{}", reply.body);
}

#[tokio::test]
async fn the_editor_offers_the_look_and_saves_it() {
    let app = setup(MonitorState::Up, true).await;
    let mut client = app.signed_in("alice", 1001).await;
    client
        .get("/admin/pages/new")
        .await
        .assert_contains(r#"name="look""#)
        .assert_contains("8-bit");

    let mut form = page_form("team", "## Core\napi | API\n");
    form.push(("look", "clean".to_owned()));
    let created = client.post_form("/admin/pages", &fields(&form)).await;
    assert_eq!(created.status, StatusCode::SEE_OTHER, "{}", created.body);
    let id: i64 = created
        .location()
        .unwrap()
        .trim_start_matches("/admin/pages/")
        .parse()
        .unwrap();
    let saved = app.db.store().page(id).await.unwrap().unwrap();
    assert_eq!(saved.spec.look, uptime_domain::Look::Clean);
    client
        .get(created.location().unwrap())
        .await
        .assert_contains(r#"value="clean" selected"#);
}

// ── Admin editor ─────────────────────────────────────────────────────────

fn page_form(slug: &str, layout: &str) -> Vec<(&'static str, String)> {
    vec![
        ("slug", slug.to_owned()),
        ("title", "Team Status".to_owned()),
        ("description", String::new()),
        ("accent", "#0ea5e9".to_owned()),
        ("theme", "auto".to_owned()),
        ("published", "on".to_owned()),
        ("layout", layout.to_owned()),
    ]
}

fn fields<'a>(pairs: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    pairs.iter().map(|(k, v)| (*k, v.as_str())).collect()
}

#[tokio::test]
async fn admins_list_create_edit_and_delete_pages() {
    let app = setup(MonitorState::Up, true).await;
    let mut client = app.signed_in("alice", 1001).await;
    client
        .get("/admin/pages")
        .await
        .assert_contains("Platform Status")
        .assert_contains("/s/platform");

    let created = client
        .post_form(
            "/admin/pages",
            &fields(&page_form("team", "## Core\napi | API\n")),
        )
        .await;
    assert_eq!(created.status, StatusCode::SEE_OTHER, "{}", created.body);
    let edit_url = created.location().unwrap().to_owned();
    let id: i64 = edit_url
        .trim_start_matches("/admin/pages/")
        .parse()
        .unwrap();
    assert_eq!(
        app.db.store().page(id).await.unwrap().unwrap().spec.title,
        "Team Status"
    );

    client.get(&edit_url).await.assert_contains("api | API");
    let updated = client
        .post_form(&edit_url, &fields(&page_form("team", "## Core\nweb\n")))
        .await;
    assert_eq!(updated.location(), Some(edit_url.as_str()));
    assert_eq!(
        app.db
            .store()
            .page(id)
            .await
            .unwrap()
            .unwrap()
            .spec
            .sections[0]
            .components[0]
            .monitor
            .as_str(),
        "web"
    );

    let deleted = client.post_form(&format!("{edit_url}/delete"), &[]).await;
    assert_eq!(deleted.location(), Some("/admin/pages"));
    assert_eq!(app.db.store().page(id).await.unwrap(), None);
}

#[tokio::test]
async fn page_layout_errors_are_shown() {
    let app = setup(MonitorState::Up, true).await;
    let mut client = app.signed_in("alice", 1001).await;

    let unknown = client
        .post_form(
            "/admin/pages",
            &fields(&page_form("team", "## Core\nmissing-monitor\n")),
        )
        .await;
    assert_eq!(unknown.status, StatusCode::UNPROCESSABLE_ENTITY);
    unknown.assert_contains("missing-monitor");

    let malformed = client
        .post_form("/admin/pages", &fields(&page_form("team", "api\n")))
        .await;
    assert_eq!(malformed.status, StatusCode::UNPROCESSABLE_ENTITY);
    malformed.assert_contains("Line 1");

    let taken = client
        .post_form("/admin/pages", &fields(&page_form("platform", "## Core\n")))
        .await;
    taken.assert_contains("already uses the slug");
}

#[tokio::test]
async fn the_page_editor_requires_signing_in() {
    let app = TestApp::new().await;
    assert_eq!(
        app.client().get("/admin/pages").await.location(),
        Some("/login")
    );
}

#[tokio::test]
async fn admins_can_download_everything_as_toml() {
    let app = setup(MonitorState::Up, true).await;
    let reply = app
        .signed_in("alice", 1001)
        .await
        .get("/admin/export.toml")
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(
        reply.headers["content-disposition"]
            .to_str()
            .unwrap()
            .contains("uptimestatus.toml")
    );
    let file = uptime_domain::ConfigFile::from_toml(&reply.body).unwrap();
    assert_eq!(file.monitors.len(), 2);
    assert_eq!(file.pages[0].slug.as_str(), "platform");
    assert_eq!(
        app.client().get("/admin/export.toml").await.location(),
        Some("/login")
    );
}

#[tokio::test]
async fn atom_feed_lists_incidents_with_their_updates() {
    let app = setup(MonitorState::Down, true).await;
    app.db
        .store()
        .declare_incident(
            &uptime_store::NewIncident {
                title: "API <slow>".into(),
                impact: uptime_domain::Impact::Major,
                status: uptime_domain::IncidentStatus::Investigating,
                message: "Looking into it.".into(),
                monitors: vec!["api".parse().unwrap()],
            },
            Timestamp::now(),
        )
        .await
        .unwrap();

    let reply = app.client().get("/s/platform/feed.atom").await;

    assert_eq!(reply.status, StatusCode::OK);
    assert!(
        reply.headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("application/atom+xml")
    );
    reply
        .assert_contains("<feed xmlns=\"http://www.w3.org/2005/Atom\">")
        .assert_contains("[Investigating] API &lt;slow&gt;")
        .assert_contains("Looking into it.")
        .assert_contains("/s/platform/incidents/");
    assert!(!reply.body.contains("secret-"));
}

#[tokio::test]
async fn atom_feed_of_a_draft_page_is_hidden() {
    let app = setup(MonitorState::Up, false).await;

    let reply = app.client().get("/s/platform/feed.atom").await;

    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn atom_feed_is_served_at_the_root_of_a_custom_domain() {
    let app = setup(MonitorState::Up, true).await;
    app.state
        .domains
        .replace([("status.team.dev".to_owned(), "platform".to_owned())]);

    let reply = app.client().get_on("status.team.dev", "/feed.atom").await;

    assert_eq!(reply.status, StatusCode::OK);
    reply.assert_contains("<link rel=\"self\" href=\"https://status.team.dev/feed.atom\"/>");
}

#[tokio::test]
async fn components_show_windowed_uptime_and_a_latency_chart() {
    let app = setup(MonitorState::Up, true).await;

    let reply = app.client().get("/s/platform").await;

    reply
        .assert_contains("24 hours")
        .assert_contains("7 days")
        .assert_contains("30 days")
        .assert_contains("Response time, last 24 hours")
        .assert_contains(r#"class="chart""#)
        .assert_contains("median 20ms")
        // The hover: per-bucket figures, a guide, two markers and a card.
        .assert_contains("data-points=")
        .assert_contains("chart-guide")
        .assert_contains("chart-dot")
        .assert_contains(r#"class="chart-tip""#);
}

#[tokio::test]
async fn pages_advertise_their_feed_to_readers() {
    let app = setup(MonitorState::Up, true).await;

    let reply = app.client().get("/s/platform").await;

    reply.assert_contains(
        r#"<link rel="alternate" type="application/atom+xml" title="Platform Status" href="/s/platform/feed.atom">"#,
    );
    let custom = app.client().get_on("status.team.dev", "/").await;
    assert_eq!(
        custom.status,
        StatusCode::NOT_FOUND,
        "unmapped hosts stay hidden"
    );
}
