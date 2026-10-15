#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! Monthly uptime reports: public pages and PDFs, private reports opened by
//! links, and their settings in the console.

mod common;

use axum::http::{StatusCode, header};
use common::TestApp;
use jiff::{SignedDuration, Timestamp};
use pretty_assertions::assert_eq;
use uptime_domain::{
    CheckPolicy, CheckSpec, ComponentSpec, ComponentStatus, Health, Impact, IncidentStatus,
    MonitorSpec, MonitorState, Month, PageSpec, Runtime, SectionSpec, TcpCheck, Theme, Verdict,
};
use uptime_store::{AuditQuery, CheckRecord, NewIncident};

/// Monitor `api` and manual component `cdn` on the published page
/// `platform`, with checks, an excused failure, an incident and a degraded
/// spell this month. Returns the app, the page id and this month.
async fn setup() -> (TestApp, i64, Month) {
    let app = TestApp::new().await;
    let store = app.db.store();
    let api = store
        .create_monitor(
            &MonitorSpec {
                key: "api".parse().unwrap(),
                name: "Public API".into(),
                check: CheckSpec::Tcp(TcpCheck::connect("api.example.com", 443)),
                policy: CheckPolicy::default(),
                active: true,
                tags: Vec::new(),
                group: None,
            },
            Timestamp::now(),
        )
        .await
        .unwrap();
    let page = store
        .create_page(&PageSpec {
            slug: "platform".parse().unwrap(),
            title: "Platform Status".into(),
            description: None,
            accent: Some("#8c6bff".parse().unwrap()),
            theme: Theme::Auto,
            look: Default::default(),
            published: true,
            website: None,
            custom_css: None,
            sections: vec![SectionSpec {
                name: "Core".into(),
                components: vec![
                    ComponentSpec::of_monitor("api".parse().unwrap(), None),
                    ComponentSpec::manual("cdn".parse().unwrap(), Some("CDN".into()), None),
                ],
            }],
        })
        .await
        .unwrap();
    // Checks in the last few minutes: three up, one down (later excused).
    let start = Timestamp::from_second(Timestamp::now().as_second() - 300).unwrap();
    for (offset, state) in [
        (0, MonitorState::Up),
        (60, MonitorState::Down),
        (120, MonitorState::Up),
        (180, MonitorState::Up),
    ] {
        let at = start + SignedDuration::from_secs(offset);
        let down = state == MonitorState::Down;
        store
            .record_check(&CheckRecord {
                monitor_id: api.id,
                scheduled_for: at,
                checked_at: at,
                verdict: Verdict {
                    health: if down { Health::Down } else { Health::Up },
                    latency: Some(std::time::Duration::from_millis(20)),
                    status_code: Some(if down { 503 } else { 200 }),
                    reason: down.then_some(uptime_domain::DownReason::UnexpectedStatus(503)),
                    response_body: None,
                },
                runtime: Runtime {
                    state,
                    consecutive_failures: u32::from(down),
                },
                transition: None,
                cert: None,
                next_run_at: at + SignedDuration::from_secs(60),
                region: "fra".into(),
            })
            .await
            .unwrap();
    }
    store
        .excuse_checks(
            api.id,
            start,
            start + SignedDuration::from_secs(90),
            Timestamp::now(),
        )
        .await
        .unwrap();
    store
        .declare_incident(
            &NewIncident {
                title: "Slow API responses".into(),
                impact: Impact::Minor,
                status: IncidentStatus::Investigating,
                message: "Looking.".into(),
                monitors: vec!["api".parse().unwrap()],
            },
            start,
        )
        .await
        .unwrap();
    store
        .set_component_status(
            page.id,
            "cdn",
            Some(ComponentStatus::Degraded),
            None,
            "alice",
            start,
        )
        .await
        .unwrap();
    (app, page.id, Month::of(start))
}

fn header_of(reply: &common::Reply, name: header::HeaderName) -> &str {
    reply
        .headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
}

#[tokio::test]
async fn public_reports_list_months_and_render_as_html_and_pdf() {
    let (app, _, month) = setup().await;
    let mut visitor = app.client();
    visitor
        .get("/s/platform")
        .await
        .assert_contains(r#"href="/s/platform/reports""#);

    let list = visitor.get("/s/platform/reports").await;
    assert_eq!(list.status, StatusCode::OK);
    list.assert_contains(&month.label())
        .assert_contains(&format!("/s/platform/reports/{month}.pdf"));
    assert!(header_of(&list, header::CACHE_CONTROL).starts_with("public"));

    let report = visitor.get(&format!("/s/platform/reports/{month}")).await;
    assert_eq!(report.status, StatusCode::OK, "{}", report.body);
    report
        .assert_contains("Uptime report")
        .assert_contains("Public API")
        .assert_contains("CDN")
        .assert_contains("100.00%")
        .assert_contains("Slow API responses")
        .assert_contains("1 failed checks were excluded")
        .assert_contains("month to date")
        .assert_contains("How uptime is measured");

    let pdf = visitor
        .get(&format!("/s/platform/reports/{month}.pdf"))
        .await;
    assert_eq!(pdf.status, StatusCode::OK);
    assert_eq!(header_of(&pdf, header::CONTENT_TYPE), "application/pdf");
    assert!(
        header_of(&pdf, header::CONTENT_DISPOSITION)
            .contains(&format!("platform-uptime-{month}.pdf"))
    );
    assert!(pdf.body.starts_with("%PDF-"));
    // Served again (from the cache) just the same.
    assert!(
        visitor
            .get(&format!("/s/platform/reports/{month}.pdf"))
            .await
            .body
            .starts_with("%PDF-")
    );

    for missing in [
        format!("/s/platform/reports/{}", month.next()),
        "/s/platform/reports/2026-13".to_owned(),
        "/s/platform/reports/soon.pdf".to_owned(),
        format!("/s/nope/reports/{month}"),
    ] {
        assert_eq!(
            visitor.get(&missing).await.status,
            StatusCode::NOT_FOUND,
            "{missing}"
        );
    }
}

#[tokio::test]
async fn private_reports_open_with_links_until_revoked() {
    let (app, page, month) = setup().await;
    let store = app.db.store();
    let mut admin = app.signed_in("alice", 1001).await;
    let console = format!("/admin/pages/{page}/reports");

    // A bad target is explained and nothing changes.
    let bad = admin
        .post_form(
            &format!("{console}/settings"),
            &[("visibility", "private"), ("sla_target", "120")],
        )
        .await;
    assert!(bad.location().unwrap().contains("error="));
    assert!(!store.report_settings(page).await.unwrap().private);

    let saved = admin
        .post_form(
            &format!("{console}/settings"),
            &[("visibility", "private"), ("sla_target", "99.9")],
        )
        .await;
    assert!(saved.location().unwrap().contains("notice="));
    let settings = store.report_settings(page).await.unwrap();
    assert!(settings.private);
    assert_eq!(settings.sla_target.unwrap().to_string(), "99.9%");

    let mut visitor = app.client();
    assert!(
        !visitor
            .get("/s/platform")
            .await
            .body
            .contains(r#"href="/s/platform/reports""#)
    );
    assert_eq!(
        visitor.get("/s/platform/reports").await.status,
        StatusCode::NOT_FOUND
    );
    let html = format!("/s/platform/reports/{month}");
    assert_eq!(visitor.get(&html).await.status, StatusCode::NOT_FOUND);

    // Admins still read them, with the target.
    let as_admin = admin.get(&html).await;
    assert_eq!(as_admin.status, StatusCode::OK);
    as_admin.assert_contains("99.9%").assert_contains("Met");
    assert!(header_of(&as_admin, header::CACHE_CONTROL).contains("no-store"));
    admin
        .get(&console)
        .await
        .assert_contains(&month.label())
        .assert_contains("Report links");

    // A link for this month only.
    let created = admin
        .post_form(
            &format!("{console}/links"),
            &[
                ("month", &month.to_string()),
                ("label", "Acme"),
                ("expires_in_days", "30"),
            ],
        )
        .await;
    assert_eq!(created.status, StatusCode::OK);
    created.assert_contains("It is not shown again");
    let marker = "?key=";
    let start = created.body.find(marker).unwrap() + marker.len();
    let key: String = created.body[start..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    assert!(key.starts_with("rpt_"), "{key}");

    let opened = visitor.get(&format!("{html}?key={key}")).await;
    assert_eq!(opened.status, StatusCode::OK, "{}", opened.body);
    opened.assert_contains(&format!("{month}.pdf?key={key}"));
    assert!(
        visitor
            .get(&format!("{html}.pdf?key={key}"))
            .await
            .body
            .starts_with("%PDF-")
    );
    for denied in [
        format!("/s/platform/reports/{}?key={key}", month.previous()),
        format!("/s/platform/reports?key={key}"),
        format!("{html}?key=rpt_wrong"),
    ] {
        assert_eq!(
            visitor.get(&denied).await.status,
            StatusCode::NOT_FOUND,
            "{denied}"
        );
    }

    let link = store.report_links(page).await.unwrap().remove(0);
    assert_eq!(link.label.as_deref(), Some("Acme"));
    let revoked = admin
        .post_form(&format!("{console}/links/{}/revoke", link.id), &[])
        .await;
    assert!(revoked.location().unwrap().contains("notice="));
    assert_eq!(
        visitor.get(&format!("{html}?key={key}")).await.status,
        StatusCode::NOT_FOUND
    );

    let actions: Vec<String> = store
        .audit_events(&AuditQuery {
            action: Some("report".into()),
            limit: 10,
            ..AuditQuery::default()
        })
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.action)
        .collect();
    assert_eq!(
        actions,
        [
            "report.link_revoke",
            "report.link_create",
            "report.settings"
        ]
    );
    let events = store
        .audit_events(&AuditQuery {
            limit: 50,
            ..AuditQuery::default()
        })
        .await
        .unwrap();
    assert!(
        events.iter().all(|e| !e.details.to_string().contains(&key)),
        "link keys never reach the audit trail"
    );
}

#[tokio::test]
async fn links_for_every_month_list_them() {
    let (app, page, month) = setup().await;
    app.db
        .store()
        .set_report_settings(
            page,
            &uptime_store::ReportSettings {
                private: true,
                sla_target: None,
            },
        )
        .await
        .unwrap();
    let mut admin = app.signed_in("alice", 1001).await;
    let created = admin
        .post_form(
            &format!("/admin/pages/{page}/reports/links"),
            &[("month", ""), ("label", ""), ("expires_in_days", "")],
        )
        .await;
    let start = created.body.find("/reports?key=").unwrap() + "/reports?key=".len();
    let key: String = created.body[start..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    let mut visitor = app.client();
    let list = visitor.get(&format!("/s/platform/reports?key={key}")).await;
    assert_eq!(list.status, StatusCode::OK);
    list.assert_contains(&format!("/s/platform/reports/{month}?key={key}"));
    let bad_expiry = admin
        .post_form(
            &format!("/admin/pages/{page}/reports/links"),
            &[("month", ""), ("label", ""), ("expires_in_days", "0")],
        )
        .await;
    bad_expiry.assert_contains("Expiry is a number of days");
}
