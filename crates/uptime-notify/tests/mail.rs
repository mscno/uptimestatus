#![allow(clippy::unwrap_used)] // tests fail loudly

//! Email through Lettermint, against a mock API.

use std::time::Duration;

use pretty_assertions::assert_eq;
use serde_json::{Value, json};
use uptime_domain::{Alert, ChannelKind, ChannelSpec, DomainState, EmailAddress};
use uptime_notify::{Email, Mailer, MailerConfig, Sender, signature, verify_webhook};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path, query_param},
};

fn mailer(server: &MockServer, team: bool) -> Mailer {
    Mailer::new(MailerConfig {
        project_token: "lm_project_test".into(),
        team_token: team.then(|| "lm_team_test".to_owned()),
        default_from: "Acme Alerts <alerts@acme.com>".parse().unwrap(),
        api_url: Some(server.uri()),
    })
    .unwrap()
}

fn email() -> Email {
    Email {
        from: "Acme Status <status@mail.acme.com>".parse().unwrap(),
        to: vec!["reader@example.com".parse().unwrap()],
        reply_to: Some("support@acme.com".parse().unwrap()),
        subject: "[Acme] Investigating: API errors".into(),
        html: "<p>hi</p>".into(),
        text: "hi".into(),
        headers: vec![
            (
                "List-Unsubscribe".into(),
                "<https://status.acme.com/u?token=x>".into(),
            ),
            (
                "List-Unsubscribe-Post".into(),
                "List-Unsubscribe=One-Click".into(),
            ),
        ],
        idempotency_key: "sub-42".into(),
        tag: "status-update".into(),
    }
}

async fn accept_sends(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/send"))
        .respond_with(
            ResponseTemplate::new(202)
                .set_body_json(json!({ "message_id": "m-1", "status": "pending" })),
        )
        .mount(server)
        .await;
}

async fn sent(server: &MockServer) -> Vec<(Value, wiremock::Request)> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path() == "/send")
        .map(|r| (serde_json::from_slice(&r.body).unwrap(), r))
        .collect()
}

#[tokio::test]
async fn sends_with_sender_headers_and_an_idempotency_key() {
    let server = MockServer::start().await;
    accept_sends(&server).await;
    mailer(&server, false).send(&email()).await.unwrap();
    let requests = sent(&server).await;
    let (body, request) = &requests[0];
    assert_eq!(
        request.headers.get("x-lettermint-token").unwrap(),
        "lm_project_test"
    );
    assert_eq!(request.headers.get("idempotency-key").unwrap(), "sub-42");
    assert_eq!(body["from"], "\"Acme Status\" <status@mail.acme.com>");
    assert_eq!(body["to"], json!(["reader@example.com"]));
    assert_eq!(body["reply_to"], json!(["support@acme.com"]));
    assert_eq!(body["subject"], "[Acme] Investigating: API errors");
    assert_eq!(body["html"], "<p>hi</p>");
    assert_eq!(body["text"], "hi");
    assert_eq!(
        body["headers"]["List-Unsubscribe-Post"],
        "List-Unsubscribe=One-Click"
    );
    assert_eq!(body["tag"], "status-update");
}

#[tokio::test]
async fn failures_are_classified_without_quoting_the_api() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/send"))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({
            "message": "The recipient reader@example.com is not permitted.",
            "errors": { "to": ["The recipient reader@example.com is not permitted."] }
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/send"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "30")
                .set_body_json(json!({ "message": "Slow down" })),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/send"))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({ "message": "busy" })))
        .mount(&server)
        .await;
    let mailer = mailer(&server, false);
    let rejected = mailer.send(&email()).await.unwrap_err();
    assert!(rejected.permanent);
    assert!(
        !rejected.message.contains("reader@"),
        "{}",
        rejected.message
    );
    assert!(rejected.message.contains("422"));
    let limited = mailer.send(&email()).await.unwrap_err();
    assert!(!limited.permanent);
    assert_eq!(limited.retry_after, Some(Duration::from_secs(30)));
    let busy = mailer.send(&email()).await.unwrap_err();
    assert!(!busy.permanent && busy.retry_after.is_none());
}

fn domain_json(status: &str) -> Value {
    let record =
        |id: &str, kind: &str, fqdn: &str, content: &str, purpose: &str, required: bool| {
            json!({
                "id": id, "type": kind, "hostname": fqdn.split('.').next().unwrap(), "fqdn": fqdn,
                "content": content, "status": status, "purpose": purpose,
                "verification_scope": if required { "required" } else { "recommended" },
                "required_for_verification": required, "verified_at": null, "last_checked_at": null,
            })
        };
    json!({
        "id": "dom_1",
        "domain": "mail.acme.com",
        "dkim_mode": "cname",
        "rotation_ready": true,
        "status_changed_at": null,
        "created_at": "2026-10-15T10:00:00Z",
        "dns_records": [
            record("r1", "CNAME", "lm1._domainkey.mail.acme.com", "lm1.dkim.lmta.net", "dkim_primary", true),
            record("r2", "CNAME", "lm-bounces.mail.acme.com", "bounces.lmta.net", "return_path", true),
            record("r3", "TXT", "_dmarc.mail.acme.com", "v=DMARC1; p=none; aspf=r", "dmarc", false),
        ],
    })
}

#[tokio::test]
async fn sending_domains_are_registered_and_verified_with_the_team_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/domains"))
        .and(header("authorization", "Bearer lm_team_test"))
        .respond_with(ResponseTemplate::new(201).set_body_json(domain_json("pending")))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/domains/dom_1"))
        .and(query_param("include", "dnsRecords"))
        .respond_with(ResponseTemplate::new(200).set_body_json(domain_json("pending")))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    let mailer = mailer(&server, true);
    assert!(mailer.manages_domains());
    let domain = mailer.register_domain("mail.acme.com").await.unwrap();
    assert_eq!(domain.id, "dom_1");
    assert_eq!(domain.status, DomainState::Pending);
    let names: Vec<&str> = domain.records.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "lm1._domainkey.mail.acme.com",
            "lm-bounces.mail.acme.com",
            "_dmarc.mail.acme.com"
        ]
    );
    assert_eq!(domain.records[0].kind, "CNAME");
    assert!(domain.records[0].required && !domain.records[2].required);

    // Records still pending: verification answers 422, which is not an error.
    Mock::given(method("POST"))
        .and(path("/domains/dom_1/dns-records/verify"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "message": "Verified", "recommended_failed_records": []
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/domains/dom_1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(domain_json("active")))
        .mount(&server)
        .await;
    let verified = mailer.verify_domain("dom_1").await.unwrap();
    assert_eq!(verified.status, DomainState::Verified);
}

#[tokio::test]
async fn domains_registered_elsewhere_are_found() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/domains"))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({
            "message": "The domain has already been taken.",
            "errors": { "domain": ["The domain has already been taken."] }
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/domains"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{ "id": "dom_1", "domain": "mail.acme.com", "status": "verified",
                       "dkim_mode": "cname", "status_changed_at": null,
                       "created_at": "2026-10-15T10:00:00Z" }],
            "path": null, "per_page": 15, "next_cursor": null, "next_page_url": null,
            "prev_cursor": null, "prev_page_url": null
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/domains/dom_1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(domain_json("active")))
        .mount(&server)
        .await;
    let domain = mailer(&server, true)
        .register_domain("mail.acme.com")
        .await
        .unwrap();
    assert_eq!(
        (domain.id.as_str(), domain.status),
        ("dom_1", DomainState::Verified)
    );
}

#[tokio::test]
async fn email_channels_send_alerts_through_the_mailer() {
    let server = MockServer::start().await;
    accept_sends(&server).await;
    let recipients: Vec<EmailAddress> = vec![
        "ops@acme.com".parse().unwrap(),
        "oncall@acme.com".parse().unwrap(),
    ];
    let channel = ChannelSpec {
        name: "On call".into(),
        kind: ChannelKind::Email,
        url: ChannelSpec::email_url(&recipients),
        secret: None,
        default_on: false,
        routing: Default::default(),
    };
    let alert = Alert::test(jiff::Timestamp::from_second(1_800_000_000).unwrap());
    let without = Sender::new(reqwest::Client::new());
    let error = without.send(&channel, &alert).await.unwrap_err();
    assert!(error.permanent && error.message.contains("not configured"));

    let sender = Sender::new(reqwest::Client::new())
        .with_app_url("https://console.acme.com".parse().unwrap())
        .with_mailer(mailer(&server, false));
    sender.send(&channel, &alert).await.unwrap();
    sender.send(&channel, &alert).await.unwrap();
    let requests = sent(&server).await;
    let (body, request) = &requests[0];
    assert_eq!(body["from"], "\"Acme Alerts\" <alerts@acme.com>");
    assert_eq!(body["to"], json!(["ops@acme.com", "oncall@acme.com"]));
    assert_eq!(body["subject"], alert.headline());
    assert!(
        body["html"]
            .as_str()
            .unwrap()
            .contains("https://console.acme.com/admin")
    );
    // Retries of the same alert reuse the key.
    assert_eq!(
        request.headers.get("idempotency-key"),
        requests[1].1.headers.get("idempotency-key")
    );
}

#[test]
fn lettermint_webhooks_are_verified() {
    let body = br#"{"id":"d1","event":"message.hard_bounced","timestamp":"2026-10-15T10:00:00Z","data":{"recipient":"gone@example.com"}}"#;
    let now = jiff::Timestamp::now().as_second();
    let v1 = signature("whsec_test", now, body);
    let header = format!("t={now},v1={}", v1.trim_start_matches("sha256="));
    let delivery = now.to_string();
    let (event, data) = verify_webhook("whsec_test", body, Some(&header), Some(&delivery)).unwrap();
    assert_eq!(event, "message.hard_bounced");
    assert_eq!(data["recipient"], "gone@example.com");
    assert!(verify_webhook("other", body, Some(&header), Some(&delivery)).is_err());
    assert!(verify_webhook("whsec_test", body, None, Some(&delivery)).is_err());
}
