#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! Custom-domain verification against a scripted DNS/HTTPS probe.

use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

use jiff::Timestamp;
use pretty_assertions::assert_eq;
use uptime_domain::{DnsAnswers, Edge, Hostname, PageSlug, PageSpec, Theme};
use uptime_runtime::{DomainProbe, DomainVerifier, VerifierConfig, VerifyDomain};
use uptime_store::{CustomDomain, DomainStatus, Store};
use uptime_testkit::TestDb;

const EDGE_IP: &str = "203.0.113.10";

#[derive(Default)]
struct Script {
    dns: HashMap<String, Result<DnsAnswers, String>>,
    https: HashMap<String, Result<(), String>>,
    dns_calls: Vec<String>,
    https_calls: Vec<String>,
}

/// DNS and HTTPS answers set per hostname; unknown names do not resolve.
#[derive(Clone, Default)]
struct FakeProbe(Arc<Mutex<Script>>);

impl FakeProbe {
    fn points_at_edge(&self, host: &str) {
        self.set_dns(
            host,
            Ok(DnsAnswers {
                cname_chain: vec!["edge.example.com".into()],
                addresses: vec![EDGE_IP.parse().unwrap()],
            }),
        );
    }

    fn points_elsewhere(&self, host: &str) {
        self.set_dns(
            host,
            Ok(DnsAnswers {
                cname_chain: vec![],
                addresses: vec!["198.51.100.7".parse().unwrap()],
            }),
        );
    }

    fn set_dns(&self, host: &str, answer: Result<DnsAnswers, String>) {
        self.0.lock().unwrap().dns.insert(host.into(), answer);
    }

    fn set_https(&self, host: &str, answer: Result<(), String>) {
        self.0.lock().unwrap().https.insert(host.into(), answer);
    }

    fn dns_calls(&self) -> Vec<String> {
        self.0.lock().unwrap().dns_calls.clone()
    }

    fn https_calls(&self) -> Vec<String> {
        self.0.lock().unwrap().https_calls.clone()
    }
}

impl DomainProbe for FakeProbe {
    fn dns(&self, host: &Hostname) -> impl Future<Output = Result<DnsAnswers, String>> + Send {
        let mut script = self.0.lock().unwrap();
        script.dns_calls.push(host.to_string());
        let answer = script
            .dns
            .get(host.as_str())
            .cloned()
            .unwrap_or(Ok(DnsAnswers::default()));
        async move { answer }
    }

    fn https(&self, host: &Hostname) -> impl Future<Output = Result<(), String>> + Send {
        let mut script = self.0.lock().unwrap();
        script.https_calls.push(host.to_string());
        let answer = script.https.get(host.as_str()).cloned().unwrap_or(Ok(()));
        async move { answer }
    }
}

type Published = Arc<Mutex<Option<Vec<(Hostname, PageSlug)>>>>;

struct Setup {
    db: TestDb,
    probe: FakeProbe,
    verifier: DomainVerifier<FakeProbe>,
    published: Published,
    page: i64,
}

fn config(prewarm_tls: bool) -> VerifierConfig {
    VerifierConfig {
        edge: Edge {
            host: "edge.example.com".into(),
            addresses: vec![EDGE_IP.parse().unwrap()],
        },
        every: Duration::from_secs(300),
        recheck_after: Duration::from_secs(24 * 60 * 60),
        prewarm_tls,
    }
}

async fn setup(prewarm_tls: bool) -> Setup {
    let db = TestDb::new().await;
    let page = db
        .store()
        .create_page(&PageSpec {
            slug: "platform".parse().unwrap(),
            title: "Platform".into(),
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
    let probe = FakeProbe::default();
    let published: Published = Arc::default();
    let sink = published.clone();
    let verifier = DomainVerifier::new(
        db.store().clone(),
        Arc::new(probe.clone()),
        config(prewarm_tls),
    )
    .with_sink(Arc::new(move |domains| {
        *sink.lock().unwrap() = Some(domains);
    }));
    Setup {
        db,
        probe,
        verifier,
        published,
        page,
    }
}

impl Setup {
    fn store(&self) -> &Store {
        self.db.store()
    }

    async fn add(&self, host: &str) -> CustomDomain {
        self.store()
            .add_domain(self.page, &host.parse().unwrap(), Timestamp::now())
            .await
            .unwrap()
    }

    async fn reload(&self, domain: &CustomDomain) -> CustomDomain {
        self.store().domain(domain.id).await.unwrap().unwrap()
    }

    fn published_hosts(&self) -> Option<Vec<String>> {
        self.published
            .lock()
            .unwrap()
            .as_ref()
            .map(|domains| domains.iter().map(|(h, _)| h.to_string()).collect())
    }
}

#[tokio::test]
async fn a_domain_pointing_at_the_edge_is_verified_published_and_prewarmed() {
    let s = setup(true).await;
    let domain = s.add("status.team.dev").await;
    s.probe.points_at_edge("status.team.dev");

    let checked = s.verifier.run_once().await.unwrap();

    assert_eq!(checked, 1);
    let domain = s.reload(&domain).await;
    assert_eq!(domain.status, DomainStatus::Verified);
    assert!(domain.cert_ok_at.is_some(), "{domain:?}");
    assert_eq!(s.published_hosts(), Some(vec!["status.team.dev".into()]));
    assert_eq!(s.probe.https_calls(), ["status.team.dev"]);
}

#[tokio::test]
async fn a_domain_pointing_elsewhere_fails_with_instructions() {
    let s = setup(true).await;
    let domain = s.add("status.team.dev").await;
    s.probe.points_elsewhere("status.team.dev");

    s.verifier.run_once().await.unwrap();

    let domain = s.reload(&domain).await;
    assert_eq!(domain.status, DomainStatus::Failed);
    let error = domain.last_error.unwrap();
    assert!(
        error.contains("198.51.100.7") && error.contains("edge.example.com"),
        "{error}"
    );
    assert!(
        s.probe.https_calls().is_empty(),
        "no HTTPS before DNS is right"
    );
}

#[tokio::test]
async fn a_domain_that_moved_away_stops_being_served() {
    let s = setup(true).await;
    let domain = s.add("status.team.dev").await;
    s.probe.points_at_edge("status.team.dev");
    s.verifier.check(&domain).await.unwrap();
    assert_eq!(s.published_hosts(), Some(vec!["status.team.dev".into()]));

    s.probe.points_elsewhere("status.team.dev");
    let checked = s.verifier.check(&s.reload(&domain).await).await.unwrap();

    assert_eq!(checked.status, DomainStatus::Failed);
    assert_eq!(s.published_hosts(), Some(vec![]));
}

#[tokio::test]
async fn a_dns_outage_does_not_unverify_a_domain() {
    let s = setup(true).await;
    let domain = s.add("status.team.dev").await;
    s.probe.points_at_edge("status.team.dev");
    s.verifier.check(&domain).await.unwrap();

    s.probe.set_dns(
        "status.team.dev",
        Err("DNS lookup failed: timed out".into()),
    );
    let checked = s.verifier.check(&s.reload(&domain).await).await.unwrap();

    assert_eq!(checked.status, DomainStatus::Verified);
}

#[tokio::test]
async fn a_dns_outage_fails_an_unverified_domain() {
    let s = setup(true).await;
    let domain = s.add("status.team.dev").await;
    s.probe.set_dns(
        "status.team.dev",
        Err("DNS lookup failed: timed out".into()),
    );

    let checked = s.verifier.check(&domain).await.unwrap();

    assert_eq!(checked.status, DomainStatus::Failed);
    assert_eq!(
        checked.last_error.as_deref(),
        Some("DNS lookup failed: timed out")
    );
}

#[tokio::test]
async fn certificate_failures_are_recorded_but_keep_the_domain() {
    let s = setup(true).await;
    let domain = s.add("status.team.dev").await;
    s.probe.points_at_edge("status.team.dev");
    s.probe
        .set_https("status.team.dev", Err("no valid certificate yet".into()));

    s.verifier.run_once().await.unwrap();

    let domain = s.reload(&domain).await;
    assert_eq!(domain.status, DomainStatus::Verified);
    assert_eq!(domain.cert_ok_at, None);
    assert_eq!(
        domain.last_error.as_deref(),
        Some("no valid certificate yet")
    );
}

#[tokio::test]
async fn prewarming_can_be_turned_off() {
    let s = setup(false).await;
    s.add("status.team.dev").await;
    s.probe.points_at_edge("status.team.dev");

    s.verifier.run_once().await.unwrap();

    assert!(s.probe.https_calls().is_empty());
}

#[tokio::test]
async fn recently_verified_domains_are_not_rechecked() {
    let s = setup(true).await;
    let fresh = s.add("fresh.team.dev").await;
    s.probe.points_at_edge("fresh.team.dev");
    s.verifier.check(&fresh).await.unwrap();
    s.add("new.team.dev").await;

    let checked = s.verifier.run_once().await.unwrap();

    assert_eq!(checked, 1);
    assert_eq!(s.probe.dns_calls(), ["fresh.team.dev", "new.team.dev"]);
}

#[tokio::test]
async fn publishing_loads_every_verified_domain() {
    let s = setup(true).await;
    let domain = s.add("status.team.dev").await;
    s.store()
        .record_domain_check(domain.id, Ok(()), Timestamp::now())
        .await
        .unwrap();

    s.verifier.publish().await.unwrap();

    assert_eq!(s.published_hosts(), Some(vec!["status.team.dev".into()]));
}

#[tokio::test]
async fn verifying_on_request_checks_dns_now_and_prewarms_in_the_background() {
    let s = setup(true).await;
    let domain = s.add("status.team.dev").await;
    s.probe.points_at_edge("status.team.dev");
    let handle: Arc<dyn VerifyDomain> = Arc::new(s.verifier.clone());

    let checked = handle.verify(domain.id).await.unwrap();

    assert_eq!(checked.status, DomainStatus::Verified);
    for _ in 0..100 {
        if s.reload(&domain).await.cert_ok_at.is_some() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the certificate was never prewarmed");
}

#[tokio::test]
async fn verifying_an_unknown_domain_fails() {
    let s = setup(true).await;
    let handle: Arc<dyn VerifyDomain> = Arc::new(s.verifier.clone());
    assert!(handle.verify(4242).await.is_err());
}
