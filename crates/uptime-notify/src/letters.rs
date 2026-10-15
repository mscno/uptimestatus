//! Email bodies: a status page's branded letters to its subscribers, and
//! plain alert emails for admins. Pure functions; every letter has an HTML
//! and a plain-text version.
//!
//! The HTML is email-safe: one centered table, inline styles, no scripts or
//! external CSS. A page's look carries over: `pixel` uses a monospaced face
//! and square corners, `clean` a system sans-serif and rounded corners, both
//! with the page's accent color.

use std::collections::HashMap;

use jiff::Timestamp;
use serde_json::{Value, json};
use uptime_domain::{
    Alert, ComponentNotice, IncidentNotice, MaintenanceNotice, MonitorKey, PageEvent,
    SubscriberKind, format_duration,
};
use url::Url;

/// A rendered email body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Letter {
    pub subject: String,
    pub html: String,
    pub text: String,
}

/// How a status page presents itself in email.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Branding {
    pub title: String,
    /// Absolute URL of the page's logo.
    pub logo_url: Option<Url>,
    /// `#rrggbb`; anything else falls back to the default.
    pub accent: Option<String>,
    /// The pixel look (monospaced, square); otherwise clean.
    pub pixel: bool,
    /// The page's public address (its custom domain when verified).
    pub page_url: Url,
    /// Change components or unsubscribe.
    pub manage_url: Option<Url>,
    /// One click, no questions.
    pub unsubscribe_url: Option<Url>,
}

impl Branding {
    fn accent(&self) -> &str {
        self.accent
            .as_deref()
            .filter(|a| valid_color(a))
            .unwrap_or("#7c5cff")
    }

    fn font(&self) -> &'static str {
        if self.pixel {
            "'JetBrains Mono', Menlo, Consolas, 'Courier New', monospace"
        } else {
            "-apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, Helvetica, Arial, sans-serif"
        }
    }

    fn radius(&self) -> &'static str {
        if self.pixel { "0" } else { "10px" }
    }

    /// A path on the page, e.g. `incidents/7`.
    fn link(&self, path: &str) -> String {
        let base = self.page_url.as_str().trim_end_matches('/');
        format!("{base}/{path}")
    }
}

fn valid_color(color: &str) -> bool {
    color.len() == 7 && color.starts_with('#') && color[1..].chars().all(|c| c.is_ascii_hexdigit())
}

/// Escapes text for HTML element content and attribute values.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

fn when(at: Timestamp) -> String {
    at.strftime("%b %-d, %Y %H:%M UTC").to_string()
}

/// The page frame around `body` (already HTML): logo, title, accent bar, footer.
fn frame(brand: &Branding, preheader: &str, body: &str) -> String {
    let accent = brand.accent();
    let logo = brand.logo_url.as_ref().map_or_else(String::new, |url| {
        format!(
            r#"<img src="{}" alt="" width="40" height="40" style="display:block;width:40px;height:40px;border:0;border-radius:{};margin-bottom:12px">"#,
            escape(url.as_str()),
            brand.radius()
        )
    });
    let mut footer = vec![format!(
        r#"<a href="{}" style="color:{accent}">View the status page</a>"#,
        escape(brand.page_url.as_str())
    )];
    if let Some(url) = &brand.manage_url {
        footer.push(format!(
            r#"<a href="{}" style="color:{accent}">Manage your subscription</a>"#,
            escape(url.as_str())
        ));
    }
    if let Some(url) = &brand.unsubscribe_url {
        footer.push(format!(
            r#"<a href="{}" style="color:{accent}">Unsubscribe</a>"#,
            escape(url.as_str())
        ));
    }
    shell(
        &Look {
            font: brand.font(),
            radius: brand.radius(),
            accent,
        },
        &brand.title,
        preheader,
        &logo,
        body,
        &footer.join(" &middot; "),
    )
}

/// Fonts and colors of a letter.
struct Look<'a> {
    font: &'a str,
    radius: &'a str,
    accent: &'a str,
}

/// The HTML document: a centered card with a heading and a footer line.
fn shell(
    look: &Look<'_>,
    title: &str,
    preheader: &str,
    logo: &str,
    body: &str,
    footer: &str,
) -> String {
    let Look {
        font,
        radius,
        accent,
    } = look;
    let title = escape(title);
    format!(
        r#"<!doctype html>
<html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{title}</title></head>
<body style="margin:0;padding:0;background:#f4f4f7;font-family:{font};color:#1f2330">
<span style="display:none;max-height:0;overflow:hidden;opacity:0">{preheader}</span>
<table role="presentation" width="100%" cellpadding="0" cellspacing="0" style="background:#f4f4f7"><tr><td align="center" style="padding:24px 12px">
<table role="presentation" width="100%" cellpadding="0" cellspacing="0" style="max-width:600px;background:#ffffff;border:1px solid #e3e5ec;border-radius:{radius};border-top:4px solid {accent}">
<tr><td style="padding:28px 28px 8px">{logo}<div style="font-size:13px;letter-spacing:.04em;text-transform:uppercase;color:#6b7080">{title}</div></td></tr>
<tr><td style="padding:8px 28px 28px;font-size:15px;line-height:1.55">{body}</td></tr>
</table>
<p style="max-width:600px;margin:16px auto 0;font-size:12px;line-height:1.6;color:#6b7080">{footer}</p>
</td></tr></table>
</body></html>"#,
        preheader = escape(preheader),
    )
}

fn button(brand: &Branding, label: &str, url: &str) -> String {
    format!(
        r#"<p style="margin:24px 0"><a href="{}" style="display:inline-block;padding:12px 20px;background:{accent};color:#ffffff;text-decoration:none;border-radius:{radius};font-weight:600">{}</a></p>"#,
        escape(url),
        escape(label),
        accent = brand.accent(),
        radius = brand.radius(),
    )
}

fn pill(brand: &Branding, label: &str, color: &str) -> String {
    format!(
        r#"<span style="display:inline-block;padding:2px 8px;border:1px solid {color};color:{color};border-radius:{};font-size:12px;font-weight:600;text-transform:uppercase;letter-spacing:.04em">{}</span>"#,
        brand.radius(),
        escape(label)
    )
}

fn paragraphs(text: &str) -> String {
    text.split("\n\n")
        .map(|p| {
            format!(
                "<p style=\"margin:0 0 12px\">{}</p>",
                escape(p).replace('\n', "<br>")
            )
        })
        .collect()
}

/// Plain-text footer lines.
fn text_footer(brand: &Branding) -> String {
    let mut lines = vec![format!("Status page: {}", brand.page_url)];
    if let Some(url) = &brand.manage_url {
        lines.push(format!("Manage your subscription: {url}"));
    }
    if let Some(url) = &brand.unsubscribe_url {
        lines.push(format!("Unsubscribe: {url}"));
    }
    format!("\n\n--\n{}\n{}\n", brand.title, lines.join("\n"))
}

/// Asks a new subscriber to confirm. Webhook subscriptions name the endpoint
/// (reduced to its host).
pub fn confirm_letter(
    brand: &Branding,
    kind: SubscriberKind,
    confirm_url: &Url,
    webhook_host: Option<&str>,
) -> Letter {
    let what = match (kind, webhook_host) {
        (SubscriberKind::Webhook, Some(host)) => {
            format!(
                "send status updates from {} to a webhook at {host}",
                brand.title
            )
        }
        _ => format!("email you status updates from {}", brand.title),
    };
    let intro = format!("Please confirm that we should {what}.");
    let ignore = "If you did not ask for this, ignore this email: nothing will be sent.";
    let html = frame(
        brand,
        &intro,
        &format!(
            "<h1 style=\"margin:0 0 12px;font-size:20px\">Confirm your subscription</h1><p style=\"margin:0 0 12px\">{}</p>{}<p style=\"margin:0;color:#6b7080;font-size:13px\">{}</p>",
            escape(&intro),
            button(brand, "Confirm subscription", confirm_url.as_str()),
            escape(ignore),
        ),
    );
    let text = format!(
        "Confirm your subscription\n\n{intro}\n\nConfirm: {confirm_url}\n\n{ignore}{}",
        text_footer(&Branding {
            manage_url: None,
            unsubscribe_url: None,
            ..brand.clone()
        })
    );
    Letter {
        subject: format!("Confirm your subscription to {}", brand.title),
        html,
        text,
    }
}

/// A test from the console: shows how the page's letters look.
pub fn test_letter(brand: &Branding) -> Letter {
    let intro = format!(
        "This is a test email from {}. Subscribers' updates look like this.",
        brand.title
    );
    let html = frame(
        brand,
        &intro,
        &format!(
            "<h1 style=\"margin:0 0 12px;font-size:20px\">Test email</h1><p style=\"margin:0\">{}</p>",
            escape(&intro)
        ),
    );
    Letter {
        subject: format!("[{}] Test email", brand.title),
        html,
        text: format!("Test email\n\n{intro}{}", text_footer(brand)),
    }
}

/// Tells a webhook's owner it was disabled after failing repeatedly.
pub fn disabled_letter(brand: &Branding, webhook_host: &str, last_error: Option<&str>) -> Letter {
    let intro = format!(
        "Your webhook at {webhook_host} kept failing, so {} stopped sending it status updates.",
        brand.title
    );
    let error = last_error.unwrap_or("unknown error");
    let resubscribe = "Fix the endpoint, then subscribe it again from the status page.";
    let html = frame(
        brand,
        &intro,
        &format!(
            "<h1 style=\"margin:0 0 12px;font-size:20px\">Webhook disabled</h1><p style=\"margin:0 0 12px\">{}</p><p style=\"margin:0 0 12px;color:#6b7080\">Last error: {}</p><p style=\"margin:0\">{}</p>",
            escape(&intro),
            escape(error),
            escape(resubscribe)
        ),
    );
    let text = format!(
        "Webhook disabled\n\n{intro}\n\nLast error: {error}\n\n{resubscribe}{}",
        text_footer(brand)
    );
    Letter {
        subject: format!("{}: webhook disabled", brand.title),
        html,
        text,
    }
}

/// Component names for an event's monitors, as the page labels them.
fn component_names(monitors: &[MonitorKey], names: &HashMap<String, String>) -> Vec<String> {
    let mut list: Vec<String> = monitors
        .iter()
        .map(|m| {
            names
                .get(m.as_str())
                .cloned()
                .unwrap_or_else(|| m.to_string())
        })
        .collect();
    list.dedup();
    list
}

fn impact_color(notice: &IncidentNotice) -> &'static str {
    use uptime_domain::{Impact, IncidentStatus};
    if notice.status == IncidentStatus::Resolved {
        return "#2f9e5b";
    }
    match notice.impact {
        Impact::None => "#3b82f6",
        Impact::Minor => "#d97706",
        Impact::Major | Impact::Critical => "#dc2626",
    }
}

/// News for a subscriber, under the page's branding. `names` maps monitor
/// keys to the page's component labels.
pub fn event_letter(
    brand: &Branding,
    event: &PageEvent,
    names: &HashMap<String, String>,
) -> Letter {
    let components = component_names(event.monitors(), names);
    let affected = if components.is_empty() {
        String::new()
    } else {
        format!("Affected: {}", components.join(", "))
    };
    let (body_html, body_text, link) = match event {
        PageEvent::IncidentCreated(n)
        | PageEvent::IncidentUpdated(n)
        | PageEvent::IncidentResolved(n) => incident_body(brand, n, &affected),
        PageEvent::MaintenanceScheduled(n)
        | PageEvent::MaintenanceStarted(n)
        | PageEvent::MaintenanceCompleted(n) => maintenance_body(brand, event, n, &affected),
        PageEvent::ComponentStatusChanged(n) => component_body(brand, n),
    };
    let headline = event.headline();
    let html = frame(
        brand,
        &headline,
        &format!(
            "<h1 style=\"margin:0 0 12px;font-size:20px\">{}</h1>{body_html}{}",
            escape(&headline),
            button(brand, "View details", &link)
        ),
    );
    let text = format!(
        "{headline}\n\n{body_text}\n\nDetails: {link}{}",
        text_footer(brand)
    );
    Letter {
        subject: format!("[{}] {headline}", brand.title),
        html,
        text,
    }
}

fn incident_body(brand: &Branding, n: &IncidentNotice, affected: &str) -> (String, String, String) {
    let mut html = format!(
        "<p style=\"margin:0 0 16px\">{} {}</p>{}",
        pill(brand, n.status.label(), impact_color(n)),
        pill(brand, &format!("{} impact", n.impact.label()), "#6b7080"),
        paragraphs(&n.message),
    );
    let mut text = format!(
        "{} · {} impact\n\n{}",
        n.status.label(),
        n.impact.label(),
        n.message
    );
    let mut facts = vec![format!("Started: {}", when(n.started_at))];
    if let Some(resolved) = n.resolved_at {
        facts.push(format!("Resolved: {}", when(resolved)));
        facts.push(format!(
            "Duration: {}",
            format_duration(resolved.duration_since(n.started_at).as_secs())
        ));
    }
    if !affected.is_empty() {
        facts.push(affected.to_owned());
    }
    html.push_str(&format!(
        "<p style=\"margin:16px 0 0;color:#6b7080;font-size:13px\">{}</p>",
        facts
            .iter()
            .map(|f| escape(f))
            .collect::<Vec<_>>()
            .join("<br>")
    ));
    text.push_str(&format!("\n\n{}", facts.join("\n")));
    (html, text, brand.link(&format!("incidents/{}", n.id)))
}

fn maintenance_body(
    brand: &Branding,
    event: &PageEvent,
    n: &MaintenanceNotice,
    affected: &str,
) -> (String, String, String) {
    let lead = match event {
        PageEvent::MaintenanceStarted(_) => "Scheduled maintenance has started.",
        PageEvent::MaintenanceCompleted(_) => "Scheduled maintenance is complete.",
        _ => "Maintenance is scheduled.",
    };
    let window = format!("{} – {}", when(n.starts_at), when(n.ends_at));
    let mut facts = vec![format!("Window: {window}")];
    if !affected.is_empty() {
        facts.push(affected.to_owned());
    }
    let description = n.description.clone().unwrap_or_default();
    let html = format!(
        "<p style=\"margin:0 0 16px\">{}</p><p style=\"margin:0 0 12px\">{}</p>{}<p style=\"margin:16px 0 0;color:#6b7080;font-size:13px\">{}</p>",
        pill(brand, "Maintenance", "#3b82f6"),
        escape(lead),
        paragraphs(&description),
        facts
            .iter()
            .map(|f| escape(f))
            .collect::<Vec<_>>()
            .join("<br>"),
    );
    let text = format!("{lead}\n\n{description}\n\n{}", facts.join("\n"));
    (html, text, brand.link("maintenance"))
}

fn component_body(brand: &Branding, n: &ComponentNotice) -> (String, String, String) {
    let status = n.status.replace('_', " ");
    let mut html = format!(
        "<p style=\"margin:0 0 12px\">{} is now {}.</p>",
        escape(&n.name),
        pill(brand, &status, "#d97706")
    );
    let mut text = format!("{} is now {status}.", n.name);
    if let Some(note) = &n.note {
        html.push_str(&paragraphs(note));
        text.push_str(&format!("\n\n{note}"));
    }
    (html, text, brand.page_url.to_string())
}

/// The JSON body sent to webhook subscribers: the event, the page, and the
/// affected components by name. `id` is unique per delivery (receivers can
/// de-duplicate retries with it).
pub fn subscriber_webhook_payload(
    id: &str,
    page_slug: &str,
    brand: &Branding,
    event: &PageEvent,
    names: &HashMap<String, String>,
) -> Value {
    let mut data = serde_json::to_value(event)
        .ok()
        .and_then(|v| v.get("data").cloned())
        .unwrap_or(Value::Null);
    if let Value::Object(object) = &mut data {
        object.insert(
            "components".into(),
            json!(component_names(event.monitors(), names)),
        );
    }
    json!({
        "id": id,
        "type": event.type_name(),
        "created_at": event.at(),
        "page": {
            "slug": page_slug,
            "title": brand.title,
            "url": brand.page_url.as_str(),
        },
        "data": data,
    })
}

/// An alert for an email channel: plain, uptimestatus-branded.
pub fn alert_letter(alert: &Alert, link: Option<&Url>) -> Letter {
    const ACCENT: &str = "#7c5cff";
    let headline = alert.headline();
    let mut facts = vec![format!("State: {}", alert.state.as_str())];
    if let Some(error) = &alert.error {
        facts.push(format!("Error: {error}"));
    }
    if let Some(code) = alert.status_code {
        facts.push(format!("Status: {code}"));
    }
    if let Some(latency) = alert.latency_ms {
        facts.push(format!("Latency: {latency} ms"));
    }
    if let Some(downtime) = alert.downtime_secs {
        facts.push(format!("Down for: {}", format_duration(downtime)));
    }
    if let Some(days) = alert.cert_days_left {
        facts.push(format!("Certificate expires in {days} days"));
    }
    facts.push(format!(
        "Monitor: {} ({})",
        alert.monitor.name, alert.monitor.key
    ));
    facts.push(format!("At: {}", when(alert.at)));
    let mut body = format!(
        "<h1 style=\"margin:0 0 12px;font-size:20px\">{}</h1><p style=\"margin:0;font-size:14px\">{}</p>",
        escape(&headline),
        facts
            .iter()
            .map(|f| escape(f))
            .collect::<Vec<_>>()
            .join("<br>")
    );
    let mut text = format!("{headline}\n\n{}", facts.join("\n"));
    if let Some(link) = link {
        body.push_str(&format!(
            r#"<p style="margin:24px 0 0"><a href="{}" style="display:inline-block;padding:12px 20px;background:{ACCENT};color:#ffffff;text-decoration:none;font-weight:600">Open in the console</a></p>"#,
            escape(link.as_str())
        ));
        text.push_str(&format!("\n\n{link}"));
    }
    let look = Look {
        font: "'JetBrains Mono', Menlo, Consolas, 'Courier New', monospace",
        radius: "0",
        accent: ACCENT,
    };
    Letter {
        subject: headline.clone(),
        html: shell(
            &look,
            "uptimestatus",
            &headline,
            "",
            &body,
            "Sent by uptimestatus",
        ),
        text,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use pretty_assertions::assert_eq;
    use uptime_domain::{Impact, IncidentStatus};

    use super::*;

    fn brand() -> Branding {
        Branding {
            title: "Acme <Status>".into(),
            logo_url: Some("https://status.acme.com/media/logo.png".parse().unwrap()),
            accent: Some("#ff3366".into()),
            pixel: false,
            page_url: "https://status.acme.com/".parse().unwrap(),
            manage_url: Some(
                "https://status.acme.com/subscriptions/manage?token=m"
                    .parse()
                    .unwrap(),
            ),
            unsubscribe_url: Some(
                "https://status.acme.com/subscriptions/unsubscribe?token=u"
                    .parse()
                    .unwrap(),
            ),
        }
    }

    fn incident(status: IncidentStatus) -> IncidentNotice {
        IncidentNotice {
            id: 7,
            title: "API <errors>".into(),
            status,
            impact: Impact::Major,
            message: "We are looking into it.\n\nMore soon & stay tuned.".into(),
            started_at: Timestamp::from_second(1_800_000_000).unwrap(),
            resolved_at: None,
            at: Timestamp::from_second(1_800_000_060).unwrap(),
            monitors: vec!["api".parse().unwrap()],
        }
    }

    #[test]
    fn incident_letters_are_branded_escaped_and_linked() {
        let names = HashMap::from([("api".to_owned(), "Public API".to_owned())]);
        let letter = event_letter(
            &brand(),
            &PageEvent::IncidentCreated(incident(IncidentStatus::Investigating)),
            &names,
        );
        assert_eq!(
            letter.subject,
            "[Acme <Status>] Investigating: API <errors>"
        );
        assert!(letter.html.contains("#ff3366"), "accent");
        assert!(
            letter
                .html
                .contains("https://status.acme.com/media/logo.png")
        );
        assert!(letter.html.contains("Acme &lt;Status&gt;"));
        assert!(letter.html.contains("API &lt;errors&gt;"));
        assert!(!letter.html.contains("<errors>"));
        assert!(letter.html.contains("More soon &amp; stay tuned."));
        assert!(letter.html.contains("https://status.acme.com/incidents/7"));
        assert!(letter.html.contains("Affected: Public API"));
        assert!(letter.html.contains("Unsubscribe"));
        assert!(letter.text.contains("Affected: Public API"));
        assert!(
            letter
                .text
                .contains("Unsubscribe: https://status.acme.com/subscriptions/unsubscribe?token=u")
        );
        assert!(
            letter
                .text
                .contains("Details: https://status.acme.com/incidents/7")
        );
    }

    #[test]
    fn looks_change_the_face_and_corners() {
        let event = PageEvent::IncidentCreated(incident(IncidentStatus::Investigating));
        let clean = event_letter(&brand(), &event, &HashMap::new());
        let pixel = event_letter(
            &Branding {
                pixel: true,
                ..brand()
            },
            &event,
            &HashMap::new(),
        );
        assert!(pixel.html.contains("monospace") && !clean.html.contains("monospace"));
        assert!(clean.html.contains("border-radius:10px"));
        // Bad accents fall back rather than reaching the CSS.
        let odd = event_letter(
            &Branding {
                accent: Some("red;background:url(x)".into()),
                ..brand()
            },
            &event,
            &HashMap::new(),
        );
        assert!(!odd.html.contains("url(x)"));
    }

    #[test]
    fn resolved_incidents_report_their_duration() {
        let mut notice = incident(IncidentStatus::Resolved);
        notice.resolved_at = Some(notice.started_at + jiff::SignedDuration::from_mins(95));
        let letter = event_letter(
            &brand(),
            &PageEvent::IncidentResolved(notice),
            &HashMap::new(),
        );
        assert_eq!(letter.subject, "[Acme <Status>] Resolved: API <errors>");
        assert!(letter.text.contains("Duration: 1h 35m"), "{}", letter.text);
    }

    #[test]
    fn confirmations_never_include_unsubscribe_links() {
        let letter = confirm_letter(
            &brand(),
            SubscriberKind::Webhook,
            &"https://status.acme.com/subscriptions/confirm?token=c"
                .parse()
                .unwrap(),
            Some("hooks.example.com"),
        );
        assert_eq!(letter.subject, "Confirm your subscription to Acme <Status>");
        assert!(letter.text.contains("a webhook at hooks.example.com"));
        assert!(
            letter
                .text
                .contains("Confirm: https://status.acme.com/subscriptions/confirm?token=c")
        );
        assert!(!letter.text.contains("Unsubscribe:"));
    }

    #[test]
    fn webhook_payloads_carry_the_event_page_and_components() {
        let names = HashMap::from([("api".to_owned(), "Public API".to_owned())]);
        let event = PageEvent::IncidentUpdated(incident(IncidentStatus::Identified));
        let payload = subscriber_webhook_payload("42", "acme", &brand(), &event, &names);
        assert_eq!(payload["id"], "42");
        assert_eq!(payload["type"], "incident.updated");
        assert_eq!(payload["page"]["slug"], "acme");
        assert_eq!(payload["page"]["url"], "https://status.acme.com/");
        assert_eq!(payload["data"]["status"], "identified");
        assert_eq!(payload["data"]["components"], json!(["Public API"]));
        assert_eq!(payload["data"]["monitors"], json!(["api"]));
    }

    #[test]
    fn alert_letters_list_the_facts() {
        let alert = Alert {
            error: Some("connection refused".into()),
            ..Alert::test(Timestamp::from_second(1_800_000_000).unwrap())
        };
        let letter = alert_letter(
            &alert,
            Some(&"https://console.example.com/admin".parse().unwrap()),
        );
        assert_eq!(letter.subject, alert.headline());
        assert!(letter.text.contains("Error: connection refused"));
        assert!(letter.html.contains("https://console.example.com/admin"));
    }
}
