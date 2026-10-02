#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! Custom domains in the page editor, and serving pages on them.

mod common;

use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::http::StatusCode;
use common::{APP_HOST, TestApp};
use jiff::Timestamp;
use pretty_assertions::assert_eq;
use uptime_domain::{DnsAnswers, Edge, Hostname, PageSpec, Theme};
use uptime_runtime::{DomainProbe, DomainVerifier, VerifierConfig};
use uptime_store::DomainStatus;

const EDGE_IP: &str = "203.0.113.10";

/// DNS answers per hostname; unknown names do not resolve. HTTPS always works.
#[derive(Clone, Default)]
struct FakeDns(Arc<Mutex<HashMap<String, DnsAnswers>>>);

impl FakeDns {
    fn point_at_edge(&self, host: &str) {
        self.0.lock().unwrap().insert(
            host.into(),
            DnsAnswers {
                cname_chain: vec!["edge.example.com".into()],
                addresses: vec![EDGE_IP.parse().unwrap()],
            },
        );
    }

    fn point_elsewhere(&self, host: &str) {
        self.0.lock().unwrap().insert(
            host.into(),
            DnsAnswers {
                cname_chain: vec!["team.github.io".into()],
                addresses: vec![],
            },
        );
    }
}

impl DomainProbe for FakeDns {
    fn dns(&self, host: &Hostname) -> impl Future<Output = Result<DnsAnswers, String>> + Send {
        let answer = self
            .0
            .lock()
            .unwrap()
            .get(host.as_str())
            .cloned()
            .unwrap_or_default();
        async move { Ok(answer) }
    }

    async fn https(&self, _host: &Hostname) -> Result<(), String> {
        Ok(())
    }
}

struct Setup {
    app: TestApp,
    dns: FakeDns,
    page: i64,
}

async fn setup() -> Setup {
    let mut app = TestApp::new().await;
    let dns = FakeDns::default();
    let edge = Edge {
        host: "edge.example.com".into(),
        addresses: vec![EDGE_IP.parse().unwrap()],
    };
    let verifier = DomainVerifier::new(
        app.db.store().clone(),
        Arc::new(dns.clone()),
        VerifierConfig {
            edge: edge.clone(),
            every: Duration::from_secs(300),
            recheck_after: Duration::from_secs(86_400),
            prewarm_tls: false,
        },
    )
    .with_sink(app.state.domains.sink());
    app.state = app
        .state
        .clone()
        .with_edge(edge)
        .with_verifier(Arc::new(verifier));
    let page = app
        .db
        .store()
        .create_page(&PageSpec {
            slug: "platform".parse().unwrap(),
            title: "Platform Status".into(),
            description: None,
            accent: None,
            theme: Theme::Auto,
            look: Default::default(),
            published: true,
            website: None,
            sections: vec![],
        })
        .await
        .unwrap()
        .id;
    Setup { app, dns, page }
}

impl Setup {
    fn url(&self, suffix: &str) -> String {
        format!("/admin/pages/{}{suffix}", self.page)
    }

    async fn domain_id(&self, host: &str) -> i64 {
        self.app
            .db
            .store()
            .domains_for_page(self.page)
            .await
            .unwrap()
            .into_iter()
            .find(|d| d.hostname.as_str() == host)
            .unwrap()
            .id
    }
}

#[tokio::test]
async fn the_editor_explains_how_to_point_dns() {
    let s = setup().await;
    let mut admin = s.app.signed_in("alice", 1001).await;

    admin
        .get(&s.url(""))
        .await
        .assert_contains("Custom domains")
        .assert_contains("edge.example.com")
        .assert_contains(EDGE_IP)
        .assert_contains(&format!("@post('{}'", s.url("/domains")));
}

#[tokio::test]
async fn added_domains_start_pending() {
    let s = setup().await;
    let mut admin = s.app.signed_in("alice", 1001).await;

    let reply = admin
        .datastar_form(&s.url("/domains"), &[("hostname", " Status.Team.dev ")])
        .await;

    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    reply
        .assert_contains("datastar-patch-elements")
        .assert_contains("status.team.dev")
        .assert_contains("Pending");
    let domains = s.app.db.store().domains_for_page(s.page).await.unwrap();
    assert_eq!(domains.len(), 1);
    assert_eq!(domains[0].status, DomainStatus::Pending);
    admin
        .get(&s.url(""))
        .await
        .assert_contains("status.team.dev");
}

#[tokio::test]
async fn bad_hostnames_are_explained() {
    let s = setup().await;
    let mut admin = s.app.signed_in("alice", 1001).await;
    admin
        .datastar_form(&s.url("/domains"), &[("hostname", "status.team.dev")])
        .await;

    for (input, message) in [
        ("https://status.team.dev/x", "not a valid hostname"),
        ("localhost", "not a valid hostname"),
        (APP_HOST, "is reserved"),
        ("edge.example.com", "is reserved"),
        ("status.team.dev", "already used"),
    ] {
        let reply = admin
            .datastar_form(&s.url("/domains"), &[("hostname", input)])
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{input}");
        reply.assert_contains(message);
    }
    assert_eq!(
        s.app
            .db
            .store()
            .domains_for_page(s.page)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn a_verified_domain_serves_the_page() {
    let s = setup().await;
    let mut admin = s.app.signed_in("alice", 1001).await;
    admin
        .datastar_form(&s.url("/domains"), &[("hostname", "status.team.dev")])
        .await;
    let id = s.domain_id("status.team.dev").await;
    let mut visitor = s.app.client();
    assert_eq!(
        visitor.get_on("status.team.dev", "/").await.status,
        StatusCode::NOT_FOUND,
        "unverified domains are not served"
    );

    s.dns.point_at_edge("status.team.dev");
    let reply = admin
        .datastar_form(&s.url(&format!("/domains/{id}/verify")), &[])
        .await;

    reply.assert_contains("Verified");
    let page = visitor.get_on("status.team.dev", "/").await;
    assert_eq!(page.status, StatusCode::OK);
    page.assert_contains("Platform Status");
}

#[tokio::test]
async fn failed_verification_says_what_to_fix() {
    let s = setup().await;
    let mut admin = s.app.signed_in("alice", 1001).await;
    admin
        .datastar_form(&s.url("/domains"), &[("hostname", "status.team.dev")])
        .await;
    let id = s.domain_id("status.team.dev").await;
    s.dns.point_elsewhere("status.team.dev");

    admin
        .datastar_form(&s.url(&format!("/domains/{id}/verify")), &[])
        .await
        .assert_contains("team.github.io")
        .assert_contains("edge.example.com");
}

#[tokio::test]
async fn removing_a_domain_stops_serving_it() {
    let s = setup().await;
    let mut admin = s.app.signed_in("alice", 1001).await;
    admin
        .datastar_form(&s.url("/domains"), &[("hostname", "status.team.dev")])
        .await;
    let id = s.domain_id("status.team.dev").await;
    s.dns.point_at_edge("status.team.dev");
    admin
        .datastar_form(&s.url(&format!("/domains/{id}/verify")), &[])
        .await;

    let reply = admin
        .datastar_form(&s.url(&format!("/domains/{id}/delete")), &[])
        .await;

    assert_eq!(reply.status, StatusCode::OK);
    reply.assert_contains("Removed status.team.dev");
    assert!(
        !reply.body.contains(&format!("id=\"domain-{id}\"")),
        "{}",
        reply.body
    );
    assert_eq!(s.app.db.store().domain(id).await.unwrap(), None);
    assert_eq!(
        s.app.client().get_on("status.team.dev", "/").await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn domains_are_managed_only_through_their_own_page() {
    let s = setup().await;
    let other = s
        .app
        .db
        .store()
        .create_page(&PageSpec {
            slug: "other".parse().unwrap(),
            title: "Other".into(),
            description: None,
            accent: None,
            theme: Theme::Auto,
            look: Default::default(),
            published: true,
            website: None,
            sections: vec![],
        })
        .await
        .unwrap()
        .id;
    let domain = s
        .app
        .db
        .store()
        .add_domain(other, &"other.team.dev".parse().unwrap(), Timestamp::now())
        .await
        .unwrap();
    let mut admin = s.app.signed_in("alice", 1001).await;

    let reply = admin
        .datastar_form(&s.url(&format!("/domains/{}/delete", domain.id)), &[])
        .await;

    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert!(s.app.db.store().domain(domain.id).await.unwrap().is_some());
}

#[tokio::test]
async fn managing_domains_requires_signing_in() {
    let s = setup().await;
    let reply = s
        .app
        .client()
        .datastar_form(&s.url("/domains"), &[("hostname", "status.team.dev")])
        .await;
    assert_ne!(reply.status, StatusCode::OK);
    assert!(
        s.app
            .db
            .store()
            .domains_for_page(s.page)
            .await
            .unwrap()
            .is_empty()
    );
}
