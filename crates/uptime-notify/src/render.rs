//! Payloads per channel kind. Pure functions of the alert.

use hmac::{Hmac, KeyInit as _, Mac as _};
use serde_json::{Value, json};
use sha2::Sha256;
use uptime_domain::{Alert, AlertEvent, format_duration};
use url::Url;

fn color(alert: &Alert) -> u32 {
    match alert.event {
        AlertEvent::WentDown | AlertEvent::Resend => 0x00e5_484d,
        AlertEvent::Recovered => 0x0030_a46c,
        AlertEvent::CertExpiring => 0x00f5_a524,
        AlertEvent::Test => 0x008e_7cf8,
    }
}

/// Label/value pairs shown under the headline.
fn facts(alert: &Alert) -> Vec<(&'static str, String)> {
    let mut facts = vec![("State", title_case(alert.state.as_str()))];
    if let Some(error) = &alert.error {
        facts.push(("Error", error.clone()));
    }
    if let Some(code) = alert.status_code {
        facts.push(("Status", code.to_string()));
    }
    if let Some(latency) = alert.latency_ms {
        facts.push(("Latency", format!("{latency} ms")));
    }
    if let Some(downtime) = alert.downtime_secs {
        facts.push(("Down for", format_duration(downtime)));
    }
    if let Some(days) = alert.cert_days_left {
        facts.push(("Certificate", format!("expires in {days} days")));
    }
    facts
}

fn title_case(word: &str) -> String {
    let mut chars = word.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// Slack mrkdwn treats `&`, `<` and `>` as control characters.
fn slack_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Slack incoming-webhook payload: a colored attachment with Block Kit blocks.
pub fn slack_payload(alert: &Alert, link: Option<&Url>) -> Value {
    let headline = slack_escape(&alert.headline());
    let title = match link {
        Some(link) => format!("*<{link}|{headline}>*"),
        None => format!("*{headline}*"),
    };
    let fields: Vec<Value> = facts(alert)
        .into_iter()
        .map(|(label, value)| json!({ "type": "mrkdwn", "text": format!("*{label}*\n{}", slack_escape(&value)) }))
        .collect();
    let mut blocks =
        vec![json!({ "type": "section", "text": { "type": "mrkdwn", "text": title } })];
    if !fields.is_empty() {
        blocks.push(json!({ "type": "section", "fields": fields }));
    }
    blocks.push(json!({
        "type": "context",
        "elements": [{
            "type": "mrkdwn",
            "text": format!(
                "`{}` · <!date^{}^{{date_short_pretty}} {{time_secs}}|{}>",
                slack_escape(&alert.monitor.key),
                alert.at.as_second(),
                alert.at
            ),
        }],
    }));
    json!({
        "text": alert.headline(),
        "attachments": [{ "color": format!("#{:06x}", color(alert)), "blocks": blocks }],
    })
}

/// Discord webhook payload: one embed. Mentions are disabled so error text
/// can never ping anyone.
pub fn discord_payload(alert: &Alert, link: Option<&Url>) -> Value {
    let fields: Vec<Value> = facts(alert)
        .into_iter()
        .map(|(label, value)| {
            let inline = label != "Error";
            json!({ "name": label, "value": truncate(&value, 1000), "inline": inline })
        })
        .collect();
    let mut embed = json!({
        "title": truncate(&alert.headline(), 250),
        "color": color(alert),
        "fields": fields,
        "timestamp": alert.at.to_string(),
        "footer": { "text": format!("monitor {}", alert.monitor.key) },
    });
    if let Some(link) = link {
        embed["url"] = json!(link.as_str());
    }
    json!({
        "username": "uptimestatus",
        "embeds": [embed],
        "allowed_mentions": { "parse": [] },
    })
}

/// Generic webhook body: the alert as-is plus a link to the monitor.
pub fn webhook_payload(alert: &Alert, link: Option<&Url>) -> Value {
    json!({
        "event": alert.event.as_str(),
        "monitor": alert.monitor,
        "state": alert.state,
        "previous_state": alert.previous_state,
        "error": alert.error,
        "latency_ms": alert.latency_ms,
        "status_code": alert.status_code,
        "at": alert.at,
        "downtime_secs": alert.downtime_secs,
        "cert_days_left": alert.cert_days_left,
        "url": link.map(Url::as_str),
    })
}

/// `sha256=<hex HMAC-SHA256(secret, "{timestamp}.{body}")>`, sent as
/// `X-Uptimestatus-Signature` alongside `X-Uptimestatus-Timestamp`.
pub fn signature(secret: &str, timestamp: i64, body: &[u8]) -> String {
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret.as_bytes()) else {
        unreachable!("HMAC accepts keys of any length")
    };
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_owned()
    } else {
        let mut cut: String = text.chars().take(max_chars.saturating_sub(1)).collect();
        cut.push('…');
        cut
    }
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;
    use pretty_assertions::assert_eq;
    use uptime_domain::{AlertMonitor, MonitorState};

    use super::*;

    fn down() -> Alert {
        Alert {
            event: AlertEvent::WentDown,
            monitor: AlertMonitor {
                id: 7,
                key: "api".into(),
                name: "Public API".into(),
            },
            state: MonitorState::Down,
            previous_state: MonitorState::Pending,
            error: Some("status 503 <not accepted>".into()),
            latency_ms: Some(120),
            status_code: Some(503),
            at: Timestamp::from_second(1_800_000_000).unwrap(),
            downtime_secs: None,
            cert_days_left: None,
        }
    }

    fn link() -> Url {
        "https://status.example.com/admin/monitors/7"
            .parse()
            .unwrap()
    }

    #[test]
    fn slack_has_a_linked_headline_escaped_fields_and_a_color() {
        let payload = slack_payload(&down(), Some(&link()));
        assert_eq!(payload["text"], "🔴 Public API is down");
        let attachment = &payload["attachments"][0];
        assert_eq!(attachment["color"], "#e5484d");
        assert_eq!(
            attachment["blocks"][0]["text"]["text"],
            "*<https://status.example.com/admin/monitors/7|🔴 Public API is down>*"
        );
        let fields = attachment["blocks"][1]["fields"].to_string();
        assert!(
            fields.contains("status 503 &lt;not accepted&gt;"),
            "{fields}"
        );
        assert!(fields.contains("120 ms"), "{fields}");
    }

    #[test]
    fn discord_has_an_embed_without_mentions() {
        let payload = discord_payload(&down(), Some(&link()));
        let embed = &payload["embeds"][0];
        assert_eq!(embed["title"], "🔴 Public API is down");
        assert_eq!(embed["color"], 0x00e5_484d);
        assert_eq!(embed["url"], link().as_str());
        assert_eq!(embed["timestamp"], "2027-01-15T08:00:00Z");
        assert_eq!(payload["allowed_mentions"]["parse"], json!([]));
    }

    #[test]
    fn recoveries_mention_the_downtime() {
        let alert = Alert {
            event: AlertEvent::Recovered,
            state: MonitorState::Up,
            previous_state: MonitorState::Down,
            error: None,
            downtime_secs: Some(11_520),
            ..down()
        };
        let payload = discord_payload(&alert, None);
        assert_eq!(payload["embeds"][0]["color"], 0x0030_a46c);
        assert!(payload.to_string().contains("3h 12m"));
        assert!(payload["embeds"][0].get("url").is_none());
    }

    #[test]
    fn webhook_payload_carries_the_whole_alert() {
        let payload = webhook_payload(&down(), Some(&link()));
        assert_eq!(payload["event"], "went_down");
        assert_eq!(payload["monitor"]["key"], "api");
        assert_eq!(payload["state"], "down");
        assert_eq!(payload["previous_state"], "pending");
        assert_eq!(payload["status_code"], 503);
        assert_eq!(payload["url"], link().as_str());
    }

    #[test]
    fn signatures_are_hmac_sha256_of_timestamp_dot_body() {
        // Reference: printf '1800000000.{"a":1}' | openssl dgst -sha256 -hmac s3cret
        assert_eq!(
            signature("s3cret", 1_800_000_000, br#"{"a":1}"#),
            "sha256=476d445fc5d734bcaf69961f0de38cb8c2ec42450829655895ca5e064c0765c9"
        );
    }

    #[test]
    fn long_text_is_truncated_for_discord() {
        let alert = Alert {
            error: Some("x".repeat(5000)),
            ..down()
        };
        let payload = discord_payload(&alert, None);
        let error = payload["embeds"][0]["fields"][1]["value"].as_str().unwrap();
        assert_eq!(error.chars().count(), 1000);
        assert!(error.ends_with('…'));
    }
}
