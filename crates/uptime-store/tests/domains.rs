#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

mod common;

use common::t;
use jiff::SignedDuration;
use pretty_assertions::assert_eq;
use uptime_domain::{Hostname, PageSpec, Theme};
use uptime_store::{DomainStatus, StoreError};
use uptime_testkit::TestDb;

fn host(name: &str) -> Hostname {
    name.parse().unwrap()
}

async fn page(db: &TestDb, slug: &str) -> i64 {
    let spec = PageSpec {
        slug: slug.parse().unwrap(),
        title: slug.into(),
        description: None,
        accent: None,
        theme: Theme::Auto,
        look: Default::default(),
        published: true,
        website: None,
        sections: vec![],
    };
    db.store().create_page(&spec).await.unwrap().id
}

#[tokio::test]
async fn domains_start_pending_and_are_listed_per_page() {
    let db = TestDb::new().await;
    let platform = page(&db, "platform").await;
    let other = page(&db, "other").await;
    db.store()
        .add_domain(platform, &host("status.team.dev"), t(0))
        .await
        .unwrap();
    db.store()
        .add_domain(platform, &host("status.acme.dev"), t(0))
        .await
        .unwrap();
    db.store()
        .add_domain(other, &host("other.acme.dev"), t(0))
        .await
        .unwrap();

    let domains = db.store().domains_for_page(platform).await.unwrap();

    let names: Vec<_> = domains.iter().map(|d| d.hostname.as_str()).collect();
    assert_eq!(names, ["status.acme.dev", "status.team.dev"]);
    assert!(domains.iter().all(|d| d.status == DomainStatus::Pending));
}

#[tokio::test]
async fn a_hostname_belongs_to_one_page() {
    let db = TestDb::new().await;
    let platform = page(&db, "platform").await;
    let other = page(&db, "other").await;
    db.store()
        .add_domain(platform, &host("status.team.dev"), t(0))
        .await
        .unwrap();

    let error = db
        .store()
        .add_domain(other, &host("status.team.dev"), t(0))
        .await
        .unwrap_err();

    assert!(
        matches!(error, StoreError::DuplicateDomain(ref h) if h.as_str() == "status.team.dev"),
        "{error:?}"
    );
}

#[tokio::test]
async fn adding_to_a_missing_page_fails() {
    let db = TestDb::new().await;
    let error = db
        .store()
        .add_domain(4242, &host("status.team.dev"), t(0))
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::PageNotFound(4242)), "{error:?}");
}

#[tokio::test]
async fn verification_results_are_recorded() {
    let db = TestDb::new().await;
    let platform = page(&db, "platform").await;
    let domain = db
        .store()
        .add_domain(platform, &host("status.team.dev"), t(0))
        .await
        .unwrap();

    let failed = db
        .store()
        .record_domain_check(domain.id, Err("not yet".into()), t(10))
        .await
        .unwrap();
    assert_eq!(
        (failed.status, failed.last_error.as_deref()),
        (DomainStatus::Failed, Some("not yet"))
    );
    assert_eq!(db.store().verified_domains().await.unwrap(), []);

    let verified = db
        .store()
        .record_domain_check(domain.id, Ok(()), t(20))
        .await
        .unwrap();
    assert_eq!(
        (verified.status, verified.last_error, verified.verified_at),
        (DomainStatus::Verified, None, Some(t(20)))
    );
    assert_eq!(verified.last_checked_at, Some(t(20)));

    let with_cert = db
        .store()
        .record_certificate(domain.id, Ok(()), t(25))
        .await
        .unwrap();
    assert_eq!(with_cert.cert_ok_at, Some(t(25)));

    let verified_domains = db.store().verified_domains().await.unwrap();
    assert_eq!(
        verified_domains,
        [(host("status.team.dev"), "platform".parse().unwrap())]
    );
}

#[tokio::test]
async fn a_verified_domain_keeps_its_first_verification_time() {
    let db = TestDb::new().await;
    let platform = page(&db, "platform").await;
    let domain = db
        .store()
        .add_domain(platform, &host("status.team.dev"), t(0))
        .await
        .unwrap();
    db.store()
        .record_domain_check(domain.id, Ok(()), t(20))
        .await
        .unwrap();

    let again = db
        .store()
        .record_domain_check(domain.id, Ok(()), t(90_000))
        .await
        .unwrap();

    assert_eq!(again.verified_at, Some(t(20)));
    assert_eq!(again.last_checked_at, Some(t(90_000)));
}

#[tokio::test]
async fn certificate_problems_are_noted_without_unverifying() {
    let db = TestDb::new().await;
    let platform = page(&db, "platform").await;
    let domain = db
        .store()
        .add_domain(platform, &host("status.team.dev"), t(0))
        .await
        .unwrap();
    db.store()
        .record_domain_check(domain.id, Ok(()), t(20))
        .await
        .unwrap();

    let result = db
        .store()
        .record_certificate(domain.id, Err("handshake failed".into()), t(21))
        .await
        .unwrap();

    assert_eq!(result.status, DomainStatus::Verified);
    assert_eq!(result.cert_ok_at, None);
    assert_eq!(result.last_error.as_deref(), Some("handshake failed"));
}

#[tokio::test]
async fn due_domains_are_unverified_ones_and_stale_verified_ones() {
    let db = TestDb::new().await;
    let platform = page(&db, "platform").await;
    let pending = db
        .store()
        .add_domain(platform, &host("pending.team.dev"), t(0))
        .await
        .unwrap();
    let fresh = db
        .store()
        .add_domain(platform, &host("fresh.team.dev"), t(0))
        .await
        .unwrap();
    let stale = db
        .store()
        .add_domain(platform, &host("stale.team.dev"), t(0))
        .await
        .unwrap();
    db.store()
        .record_domain_check(fresh.id, Ok(()), t(90_000))
        .await
        .unwrap();
    db.store()
        .record_domain_check(stale.id, Ok(()), t(0))
        .await
        .unwrap();

    let due = db
        .store()
        .domains_to_check(t(90_001), SignedDuration::from_hours(24))
        .await
        .unwrap();

    let mut ids: Vec<_> = due.iter().map(|d| d.id).collect();
    ids.sort_unstable();
    let mut expected = vec![pending.id, stale.id];
    expected.sort_unstable();
    assert_eq!(ids, expected);
}

#[tokio::test]
async fn domains_go_away_with_their_page_or_on_request() {
    let db = TestDb::new().await;
    let platform = page(&db, "platform").await;
    let first = db
        .store()
        .add_domain(platform, &host("one.team.dev"), t(0))
        .await
        .unwrap();
    db.store()
        .add_domain(platform, &host("two.team.dev"), t(0))
        .await
        .unwrap();

    assert!(db.store().remove_domain(first.id).await.unwrap());
    assert!(!db.store().remove_domain(first.id).await.unwrap());
    assert_eq!(
        db.store().domains_for_page(platform).await.unwrap().len(),
        1
    );

    db.store().delete_page(platform).await.unwrap();
    assert_eq!(db.store().domain(first.id).await.unwrap(), None);
    assert!(
        db.store()
            .domains_for_page(platform)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn checking_a_missing_domain_fails() {
    let db = TestDb::new().await;
    let error = db
        .store()
        .record_domain_check(4242, Ok(()), t(0))
        .await
        .unwrap_err();
    assert!(
        matches!(error, StoreError::DomainNotFound(4242)),
        "{error:?}"
    );
}
