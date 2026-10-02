#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

use std::time::Duration;

use jiff::Timestamp;
use pretty_assertions::assert_eq;
use uptime_domain::{Alert, AlertEvent, AlertMonitor, ChannelKind, ChannelSpec, MonitorState};
use uptime_notify::{Sender, signature};
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{header, header_exists, method, path},
};

fn alert() -> Alert {
    Alert {
        event: AlertEvent::WentDown,
        monitor: AlertMonitor {
            id: 7,
            key: "api".into(),
            name: "Public API".into(),
        },
        state: MonitorState::Down,
        previous_state: MonitorState::Pending,
        error: Some("connection refused".into()),
        latency_ms: None,
        status_code: None,
        at: Timestamp::now(),
        downtime_secs: None,
        cert_days_left: None,
    }
}

fn channel(kind: ChannelKind, url: String, secret: Option<&str>) -> ChannelSpec {
    ChannelSpec {
        name: "Ops".into(),
        kind,
        url: url.parse().unwrap(),
        secret: secret.map(str::to_owned),
        default_on: false,
        routing: Default::default(),
    }
}

fn sender() -> Sender {
    Sender::new(reqwest::Client::new()).with_app_url("https://status.example.com".parse().unwrap())
}

fn body_json(request: &Request) -> serde_json::Value {
    serde_json::from_slice(&request.body).unwrap()
}

#[tokio::test]
async fn slack_gets_a_block_kit_message_linking_to_the_monitor() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/services/T/B/x"))
        .and(header("content-type", "application/json"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .expect(1)
        .mount(&server)
        .await;

    let result = sender()
        .send(
            &channel(
                ChannelKind::Slack,
                format!("{}/services/T/B/x", server.uri()),
                None,
            ),
            &alert(),
        )
        .await;

    assert_eq!(result, Ok(()));
    let request = &server.received_requests().await.unwrap()[0];
    let body = body_json(request);
    assert_eq!(body["text"], "🔴 Public API is down");
    assert!(
        body.to_string()
            .contains("https://status.example.com/admin/monitors/7"),
        "{body}"
    );
}

#[tokio::test]
async fn signed_webhooks_can_be_verified_by_the_receiver() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .and(header("x-uptimestatus-event", "went_down"))
        .and(header_exists("x-uptimestatus-signature"))
        .respond_with(ResponseTemplate::new(204))
        .expect(1)
        .mount(&server)
        .await;

    sender()
        .send(
            &channel(
                ChannelKind::Webhook,
                format!("{}/hook", server.uri()),
                Some("s3cret"),
            ),
            &alert(),
        )
        .await
        .unwrap();

    let request = &server.received_requests().await.unwrap()[0];
    let timestamp: i64 = request.headers["x-uptimestatus-timestamp"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        request.headers["x-uptimestatus-signature"]
            .to_str()
            .unwrap(),
        signature("s3cret", timestamp, &request.body)
    );
    assert_eq!(body_json(request)["monitor"]["key"], "api");
}

#[tokio::test]
async fn unsigned_webhooks_have_no_signature() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    sender()
        .send(&channel(ChannelKind::Webhook, server.uri(), None), &alert())
        .await
        .unwrap();

    let request = &server.received_requests().await.unwrap()[0];
    assert!(!request.headers.contains_key("x-uptimestatus-signature"));
}

#[tokio::test]
async fn rate_limits_say_how_long_to_wait() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(429)
                .set_body_json(serde_json::json!({ "message": "slow down", "retry_after": 2.5 })),
        )
        .mount(&server)
        .await;

    let error = sender()
        .send(
            &channel(
                ChannelKind::Discord,
                format!("{}/api/webhooks/1/x", server.uri()),
                None,
            ),
            &alert(),
        )
        .await
        .unwrap_err();

    assert!(!error.permanent);
    assert_eq!(error.retry_after, Some(Duration::from_millis(2500)));
}

#[tokio::test]
async fn a_deleted_webhook_is_a_permanent_failure() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(404).set_body_string("echoed-secret"))
        .mount(&server)
        .await;

    let error = sender()
        .send(&channel(ChannelKind::Webhook, server.uri(), None), &alert())
        .await
        .unwrap_err();

    assert!(error.permanent, "{error:?}");
    assert!(error.message.contains("404"), "{}", error.message);
    assert!(!error.message.contains("echoed-secret"), "{error:?}");
}

#[tokio::test]
async fn server_errors_and_timeouts_are_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(502))
        .mount(&server)
        .await;

    let error = sender()
        .send(&channel(ChannelKind::Webhook, server.uri(), None), &alert())
        .await
        .unwrap_err();

    assert!(!error.permanent);
    assert_eq!(error.retry_after, None);
}

#[tokio::test]
async fn unreachable_receivers_are_retried() {
    let error = sender()
        .send(
            &channel(
                ChannelKind::Webhook,
                "http://127.0.0.1:9/webhook-secret".into(),
                None,
            ),
            &alert(),
        )
        .await
        .unwrap_err();
    assert!(!error.permanent, "{error:?}");
    assert!(!error.message.contains("webhook-secret"), "{error:?}");
}

#[tokio::test]
async fn guarded_senders_refuse_private_addresses() {
    let prober = std::sync::Arc::new(
        uptime_probe::Prober::new(uptime_probe::ProberConfig::default()).unwrap(),
    );
    let sender = Sender::guarded(&prober, "test").unwrap();

    let error = sender
        .send(
            &channel(ChannelKind::Webhook, "http://127.0.0.1:9/hook".into(), None),
            &alert(),
        )
        .await
        .unwrap_err();

    assert!(error.permanent);
    assert!(error.message.contains("private"), "{}", error.message);
}

#[test]
fn test_alerts_link_to_the_console() {
    let link = sender().link(&Alert::test(Timestamp::now())).unwrap();
    assert_eq!(link.as_str(), "https://status.example.com/admin");
}
