#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! Status page tabs: maintenance by month, incident history by month and day,
//! and one incident's full timeline.

mod common;

use axum::http::StatusCode;
use common::TestApp;
use jiff::{SignedDuration, Timestamp, ToSpan as _, civil::Date, tz::TimeZone};
use pretty_assertions::assert_eq;
use uptime_domain::{
    CheckPolicy, CheckSpec, ComponentSpec, Impact, IncidentStatus, MaintenanceSpec, MonitorSpec,
    PageSpec, SectionSpec, TcpCheck, Theme,
};
use uptime_store::NewIncident;

fn monitor(key: &str) -> MonitorSpec {
    MonitorSpec {
        key: key.parse().unwrap(),
        name: format!("{key} service"),
        check: CheckSpec::Tcp(TcpCheck::connect(format!("{key}.example.com"), 443)),
        policy: CheckPolicy::default(),
        active: true,
        tags: Vec::new(),
        group: None,
    }
}

/// Monitors `api` and `db`; page `platform` shows only `api`, as "Public API".
async fn setup(published: bool) -> TestApp {
    let app = TestApp::new().await;
    let store = app.db.store();
    for key in ["api", "db"] {
        store
            .create_monitor(&monitor(key), Timestamp::now())
            .await
            .unwrap();
    }
    store
        .create_page(&PageSpec {
            slug: "platform".parse().unwrap(),
            title: "Platform".into(),
            description: None,
            accent: None,
            theme: Theme::Auto,
            look: Default::default(),
            published,
            website: None,
            sections: vec![SectionSpec {
                name: "Core".into(),
                components: vec![ComponentSpec {
                    monitor: "api".parse().unwrap(),
                    label: Some("Public API".into()),
                }],
            }],
        })
        .await
        .unwrap();
    app
}

/// The first day of the month `offset` months from this one (UTC).
fn month(offset: i64) -> Date {
    Timestamp::now()
        .to_zoned(TimeZone::UTC)
        .date()
        .first_of_month()
        .checked_add(offset.months())
        .unwrap()
}

/// `day` (1-based) of `month`, at `hour`:00 UTC.
fn at(month: Date, day: i8, hour: i8) -> Timestamp {
    month
        .with()
        .day(day)
        .build()
        .unwrap()
        .at(hour, 0, 0, 0)
        .to_zoned(TimeZone::UTC)
        .unwrap()
        .timestamp()
}

fn ym(month: Date) -> String {
    month.strftime("%Y-%m").to_string()
}

async fn schedule(app: &TestApp, title: &str, starts_at: Timestamp, hours: i64, key: &str) {
    app.db
        .store()
        .create_maintenance(
            &MaintenanceSpec {
                title: title.into(),
                description: Some(format!("About {title}.")),
                starts_at,
                ends_at: starts_at + SignedDuration::from_hours(hours),
                repeat: None,
                repeat_until: None,
                monitors: vec![key.parse().unwrap()],
            },
            Timestamp::now(),
        )
        .await
        .unwrap();
}

async fn declare(app: &TestApp, title: &str, started_at: Timestamp, key: &str) -> i64 {
    app.db
        .store()
        .declare_incident(
            &NewIncident {
                title: title.into(),
                impact: Impact::Major,
                status: IncidentStatus::Investigating,
                message: format!("Looking into {title}."),
                monitors: vec![key.parse().unwrap()],
            },
            started_at,
        )
        .await
        .unwrap()
        .id
}

async fn update(app: &TestApp, id: i64, status: IncidentStatus, body: &str, at: Timestamp) {
    app.db
        .store()
        .post_incident_update(id, status, body, at)
        .await
        .unwrap();
}

// ── Tabs ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn every_tab_links_to_the_others() {
    let app = setup(true).await;
    let mut client = app.client();
    for (path, current) in [
        ("/s/platform", "/s/platform"),
        ("/s/platform/maintenance", "/s/platform/maintenance"),
        ("/s/platform/incidents", "/s/platform/incidents"),
    ] {
        let page = client.get(path).await;
        assert_eq!(page.status, StatusCode::OK, "{path}");
        page.assert_contains(r#"<nav class="status-tabs""#)
            .assert_contains(r#"href="/s/platform/maintenance""#)
            .assert_contains(r#"href="/s/platform/incidents""#)
            .assert_contains(&format!(r#"href="{current}" aria-current="page""#));
    }
}

#[tokio::test]
async fn drafts_hide_every_tab() {
    let app = setup(false).await;
    let mut client = app.client();
    for path in ["/s/platform/maintenance", "/s/platform/incidents"] {
        assert_eq!(
            client.get(path).await.status,
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
}

// ── Maintenance ──────────────────────────────────────────────────────────

#[tokio::test]
async fn maintenance_shows_last_this_and_next_month() {
    let app = setup(true).await;
    let now = Timestamp::now();
    schedule(&app, "Router swap", at(month(-1), 10, 2), 2, "api").await;
    schedule(
        &app,
        "Load balancer swap",
        now - SignedDuration::from_mins(1),
        1,
        "api",
    )
    .await;
    schedule(&app, "Kernel upgrade", at(month(1), 12, 22), 2, "api").await;
    schedule(&app, "Database failover", at(month(1), 12, 22), 2, "db").await;
    schedule(&app, "Datacenter move", at(month(4), 3, 6), 4, "api").await;

    let page = app.client().get("/s/platform/maintenance").await;

    assert_eq!(page.status, StatusCode::OK);
    page.assert_contains(&format!(
        "{} to {}",
        month(-1).strftime("%b %Y"),
        month(1).strftime("%b %Y")
    ))
    .assert_contains(&month(-1).strftime("%B %Y").to_string())
    .assert_contains(&month(0).strftime("%B %Y").to_string())
    .assert_contains(&month(1).strftime("%B %Y").to_string())
    .assert_contains("Router swap")
    .assert_contains("Completed")
    .assert_contains("Load balancer swap")
    .assert_contains("In progress")
    .assert_contains("Kernel upgrade")
    .assert_contains("Scheduled")
    .assert_contains("About Kernel upgrade.")
    .assert_contains(&format!(r#"href="?from={}""#, ym(month(-4))))
    .assert_contains(&format!(r#"href="?from={}""#, ym(month(2))));
    assert!(
        !page.body.contains("Database failover"),
        "not this page's monitors"
    );
    assert!(!page.body.contains("Datacenter move"), "outside the range");

    let later = app
        .client()
        .get(&format!("/s/platform/maintenance?from={}", ym(month(3))))
        .await;
    later.assert_contains("Datacenter move");
}

#[tokio::test]
async fn empty_months_say_whether_maintenance_is_still_to_come() {
    let app = setup(true).await;
    let mut client = app.client();

    let past = client
        .get(&format!("/s/platform/maintenance?from={}", ym(month(-9))))
        .await;
    assert_eq!(past.body.matches("No maintenance</p>").count(), 3);
    assert!(!past.body.contains("No maintenance scheduled"));

    let future = client
        .get(&format!("/s/platform/maintenance?from={}", ym(month(6))))
        .await;
    assert_eq!(future.body.matches("No maintenance scheduled").count(), 3);
}

#[tokio::test]
async fn a_malformed_range_falls_back_to_the_default() {
    let app = setup(true).await;
    let page = app.client().get("/s/platform/maintenance?from=soon").await;
    assert_eq!(page.status, StatusCode::OK);
    page.assert_contains(&month(0).strftime("%B %Y").to_string());
}

// ── Incident history ─────────────────────────────────────────────────────

/// "API outage" (3 updates, resolved) and "Earlier blip" on the 4th of last
/// month, "DB trouble" on another page's monitor, and "Ancient" 5 months ago.
async fn history(app: &TestApp) -> (i64, i64) {
    let outage = declare(app, "API outage", at(month(-1), 4, 10), "api").await;
    update(
        app,
        outage,
        IncidentStatus::Identified,
        "A bad deploy; rolling back.",
        at(month(-1), 4, 11),
    )
    .await;
    update(
        app,
        outage,
        IncidentStatus::Resolved,
        "Back to normal.",
        at(month(-1), 4, 12),
    )
    .await;
    declare(app, "Earlier blip", at(month(-1), 4, 8), "api").await;
    let elsewhere = declare(app, "DB trouble", at(month(-1), 6, 8), "db").await;
    declare(app, "Ancient", at(month(-5), 2, 8), "api").await;
    (outage, elsewhere)
}

#[tokio::test]
async fn incident_history_groups_incidents_by_month_and_day() {
    let app = setup(true).await;
    let (outage, _) = history(&app).await;

    let page = app.client().get("/s/platform/incidents").await;

    assert_eq!(page.status, StatusCode::OK);
    page.assert_contains(&format!(
        "{} to {}",
        month(-2).strftime("%b %Y"),
        month(0).strftime("%b %Y")
    ))
    .assert_contains(&format!(
        "{} · 2 incidents",
        at(month(-1), 4, 0).strftime("%b %d, %Y")
    ))
    .assert_contains(&format!(r#"href="/s/platform/incidents/{outage}""#))
    .assert_contains("API outage")
    .assert_contains("pill pill-up")
    .assert_contains("Back to normal.")
    .assert_contains("2 previous updates")
    .assert_contains("Earlier blip")
    .assert_contains("No incidents reported")
    .assert_contains(&format!(r#"href="?from={}""#, ym(month(-5))));
    assert!(
        !page.body.contains("DB trouble"),
        "not this page's monitors"
    );
    assert!(!page.body.contains("Ancient"), "outside the range");
    assert!(
        !page.body.contains(&format!("?from={}", ym(month(1)))),
        "no paging into the future"
    );

    let older = app
        .client()
        .get(&format!("/s/platform/incidents?from={}", ym(month(-5))))
        .await;
    older.assert_contains("Ancient");
}

#[tokio::test]
async fn an_incident_page_shows_its_whole_timeline() {
    let app = setup(true).await;
    let (outage, elsewhere) = history(&app).await;

    let page = app
        .client()
        .get(&format!("/s/platform/incidents/{outage}"))
        .await;

    assert_eq!(page.status, StatusCode::OK);
    page.assert_contains("<title>API outage · Platform · uptimestatus</title>")
        .assert_contains(r#"href="/s/platform">"#)
        .assert_contains("Back to overview")
        .assert_contains("Resolved")
        .assert_contains("Affected services")
        .assert_contains("Public API")
        .assert_contains("Looking into API outage.")
        .assert_contains("A bad deploy; rolling back.")
        .assert_contains("Back to normal.")
        .assert_contains(&format!(r#"datetime="{}""#, at(month(-1), 4, 10)));

    let mut client = app.client();
    for path in [
        format!("/s/platform/incidents/{elsewhere}"),
        "/s/platform/incidents/999999".to_owned(),
        "/s/platform/incidents/nope".to_owned(),
    ] {
        assert_eq!(
            client.get(&path).await.status,
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
}

#[tokio::test]
async fn the_status_tab_links_incidents_to_their_pages() {
    let app = setup(true).await;
    let id = declare(&app, "Elevated errors", Timestamp::now(), "api").await;
    app.client()
        .get("/s/platform")
        .await
        .assert_contains(&format!(r#"href="/s/platform/incidents/{id}""#));
}

// ── Custom domains ───────────────────────────────────────────────────────

#[tokio::test]
async fn custom_domains_link_without_the_slug() {
    let app = setup(true).await;
    let (outage, _) = history(&app).await;
    app.state
        .domains
        .replace([("status.team.dev".to_owned(), "platform".to_owned())]);
    let mut client = app.client();

    let incidents = client.get_on("status.team.dev", "/incidents").await;
    assert_eq!(incidents.status, StatusCode::OK);
    incidents
        .assert_contains(&format!(r#"href="/incidents/{outage}""#))
        .assert_contains(r#"href="/maintenance""#)
        .assert_contains(r#"href="/""#);
    assert!(
        !incidents.body.contains("/s/platform"),
        "{}",
        incidents.body
    );

    let detail = client
        .get_on("status.team.dev", &format!("/incidents/{outage}"))
        .await;
    assert_eq!(detail.status, StatusCode::OK);
    assert!(!detail.body.contains("/s/platform"));

    let status = client.get_on("status.team.dev", "/").await;
    status.assert_contains("@get('/live')");
}
