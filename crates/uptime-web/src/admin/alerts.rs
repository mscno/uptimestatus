//! Alert channels (Slack, Discord, webhooks): list, create, edit, delete,
//! "Send test", and the recent delivery log.

use std::fmt;

use jiff::Timestamp;
use serde::Deserialize;
use topcoat::{
    Result,
    context::Cx,
    datastar::PatchElements,
    router::{
        StatusCode,
        content::{Form, Html},
        error::{SeeOther, not_found, see_other},
        page, path_param,
        response::{IntoResponse as _, Response},
        route,
    },
    view::{View, ViewExt as _, component, view},
};
use uptime_domain::{Alert, AlertEvent, ChannelKind, ChannelSpec, QuietHours, Routing};
use uptime_store::{AdminUser, Channel, DeliveryStatus, StoreError};
use url::Url;

use super::{
    Id,
    form::FormErrors,
    views::{checkbox, text_field},
};
use crate::{
    auth::{app, require_admin},
    views::{admin_shell, ago, delete_button},
};

fn channel_id(cx: &Cx) -> Result<i64> {
    Ok(*path_param::<Id>(cx)?)
}

/// The channel form as typed.
#[derive(Clone, Default, Deserialize)]
#[serde(default)]
pub(crate) struct ChannelForm {
    name: String,
    kind: String,
    url: String,
    /// Webhooks only; blank keeps the current secret when editing.
    secret: String,
    default_on: Option<String>,
    /// Minutes a monitor must be down before this channel hears about it.
    escalate_after: String,
    /// `HH:MM` (UTC); both blank means no quiet hours.
    quiet_start: String,
    quiet_end: String,
    mute_recovered: Option<String>,
    mute_resend: Option<String>,
    mute_cert: Option<String>,
}

impl fmt::Debug for ChannelForm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChannelForm")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

/// `22:30` as minutes after midnight.
fn parse_clock(text: &str) -> Option<u16> {
    let (hours, minutes) = text.trim().split_once(':')?;
    let (hours, minutes) = (hours.parse::<u16>().ok()?, minutes.parse::<u16>().ok()?);
    (hours < 24 && minutes < 60).then_some(hours * 60 + minutes)
}

fn clock(minutes: u16) -> String {
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

/// "after 10 min · quiet 22:00–06:00 · mutes recovered", or "—".
fn routing_summary(routing: &Routing) -> String {
    let mut parts = Vec::new();
    if routing.escalates() {
        parts.push(format!("after {} min", routing.escalate_after_mins));
    }
    if let Some(quiet) = routing.quiet {
        parts.push(format!(
            "quiet {}–{}",
            clock(quiet.start_min),
            clock(quiet.end_min)
        ));
    }
    let muted: Vec<&str> = routing
        .mute
        .iter()
        .map(|event| match event {
            AlertEvent::Recovered => "recovered",
            AlertEvent::Resend => "reminders",
            AlertEvent::CertExpiring => "certificates",
            _ => "other",
        })
        .collect();
    if !muted.is_empty() {
        parts.push(format!("mutes {}", muted.join(", ")));
    }
    if parts.is_empty() {
        "—".into()
    } else {
        parts.join(" · ")
    }
}

impl ChannelForm {
    fn new_channel() -> Self {
        Self {
            kind: ChannelKind::Slack.as_str().into(),
            ..Self::default()
        }
    }

    fn from_spec(spec: &ChannelSpec) -> Self {
        Self {
            name: spec.name.clone(),
            kind: spec.kind.as_str().into(),
            url: spec.url.to_string(),
            secret: String::new(),
            default_on: spec.default_on.then(|| "on".into()),
            escalate_after: if spec.routing.escalates() {
                spec.routing.escalate_after_mins.to_string()
            } else {
                String::new()
            },
            quiet_start: spec
                .routing
                .quiet
                .map(|q| clock(q.start_min))
                .unwrap_or_default(),
            quiet_end: spec
                .routing
                .quiet
                .map(|q| clock(q.end_min))
                .unwrap_or_default(),
            mute_recovered: spec
                .routing
                .mute
                .contains(&AlertEvent::Recovered)
                .then(|| "on".into()),
            mute_resend: spec
                .routing
                .mute
                .contains(&AlertEvent::Resend)
                .then(|| "on".into()),
            mute_cert: spec
                .routing
                .mute
                .contains(&AlertEvent::CertExpiring)
                .then(|| "on".into()),
        }
    }

    /// `current_secret`: what to keep when the secret field is left blank.
    fn to_spec(&self, current_secret: Option<String>) -> Result<ChannelSpec, FormErrors> {
        let mut errors = FormErrors::default();
        let kind = self.kind.parse::<ChannelKind>().unwrap_or_else(|message| {
            errors.insert("kind", message);
            ChannelKind::Webhook
        });
        let url = self.url.trim().parse::<Url>().unwrap_or_else(|_| {
            errors.insert(
                "url",
                "Enter a full URL, e.g. https://hooks.slack.com/services/…",
            );
            // Placeholder so validation below can still report the name.
            Url::parse("https://invalid.invalid/").unwrap_or_else(|_| unreachable!())
        });
        let secret = match kind {
            ChannelKind::Webhook => Some(self.secret.trim().to_owned())
                .filter(|s| !s.is_empty())
                .or(current_secret),
            _ => None,
        };
        let escalate_after_mins = match self.escalate_after.trim() {
            "" => 0,
            text => text.parse::<u32>().unwrap_or_else(|_| {
                errors.insert("escalate_after", "Enter a whole number of minutes.");
                0
            }),
        };
        let quiet = match (self.quiet_start.trim(), self.quiet_end.trim()) {
            ("", "") => None,
            (start, end) => match (parse_clock(start), parse_clock(end)) {
                (Some(start_min), Some(end_min)) => Some(QuietHours { start_min, end_min }),
                _ => {
                    errors.insert(
                        "quiet_start",
                        "Use both times as HH:MM (UTC), or leave both blank.",
                    );
                    None
                }
            },
        };
        let mute = [
            (&self.mute_recovered, AlertEvent::Recovered),
            (&self.mute_resend, AlertEvent::Resend),
            (&self.mute_cert, AlertEvent::CertExpiring),
        ]
        .into_iter()
        .filter(|(flag, _)| flag.is_some())
        .map(|(_, event)| event)
        .collect();
        let spec = ChannelSpec {
            name: self.name.trim().to_owned(),
            kind,
            url,
            secret,
            default_on: self.default_on.is_some(),
            routing: Routing {
                escalate_after_mins,
                quiet,
                mute,
            },
        };
        if let Err(error) = spec.validate() {
            // A bad URL already has its own message.
            if !(error.field() == "url" && errors.get("url").is_some()) {
                errors.insert(error.field(), error.to_string());
            }
        }
        if self.name.trim().is_empty() {
            errors.insert("name", "Give the channel a name.");
        }
        if errors.is_empty() {
            Ok(spec)
        } else {
            Err(errors)
        }
    }
}

/// `https://hooks.slack.com/…`: enough to recognise, not enough to use.
fn destination(url: &Url) -> String {
    format!(
        "{}://{}/…",
        url.scheme(),
        url.host_str().unwrap_or_default()
    )
}

#[page("/admin/alerts")]
pub(crate) async fn alerts_page(cx: &Cx) -> Result<impl View> {
    let admin = require_admin(cx).await?;
    let store = &app(cx).store;
    let channels = store.list_channels().await?;
    let deliveries = store.recent_deliveries(30).await?;
    let now = Timestamp::now();
    Ok(view! {
        admin_shell(title: "Alerts", admin: &admin, section: "alerts",
            <div class="page-head">
                <h1>"Alerts"</h1>
                <span class="spacer"></span>
                <a class="btn btn-primary" href="/admin/alerts/new">"New channel"</a>
            </div>
            if channels.is_empty() {
                <div class="card empty">
                    <p><strong>"No alert channels yet."</strong></p>
                    <p>"Add Slack, Discord or a webhook, then choose it on each monitor."</p>
                </div>
            } else {
                <div class="card table-wrap">
                    <h2>"Channels"</h2>
                    <table class="list">
                        <thead><tr><th>"Name"</th><th>"Kind"</th><th>"Destination"</th><th class="hide-sm">"Routing"</th><th class="hide-sm">"New monitors"</th><th></th><th></th></tr></thead>
                        <tbody>
                            for channel in &channels {
                                <tr>
                                    <td class="name-cell"><a href=(format!("/admin/alerts/{}", channel.id))>(channel.spec.name.as_str())</a></td>
                                    <td>(channel.spec.kind.label())</td>
                                    <td class="mono small">(destination(&channel.spec.url))</td>
                                    <td class="small hide-sm">(routing_summary(&channel.spec.routing))</td>
                                    <td class="small hide-sm">(if channel.spec.default_on { "on by default" } else { "—" })</td>
                                    <td class="small" id=(format!("test-{}", channel.id))></td>
                                    <td class="num">
                                        <button class="btn" type="button"
                                            data-on:click=(format!("@post('/admin/alerts/{}/test')", channel.id))
                                            data-indicator=(format!("_sending{}", channel.id))
                                            data-attr:aria-busy=(format!("$_sending{}", channel.id))>"Send test"</button>
                                    </td>
                                </tr>
                            }
                        </tbody>
                    </table>
                </div>
            }
            <div class="card table-wrap">
                <h2>"Recent deliveries"</h2>
                if deliveries.is_empty() {
                    <p class="muted small" style="padding: 0 0.75rem">"Nothing sent yet. Alerts are queued when a monitor goes down or recovers."</p>
                } else {
                    <table class="list">
                        <thead><tr><th>"When"</th><th>"Alert"</th><th class="hide-sm">"Channel"</th><th>"Status"</th><th class="num hide-sm">"Tries"</th><th>"Last error"</th></tr></thead>
                        <tbody>
                            for delivery in &deliveries {
                                <tr>
                                    <td class="small when">ago(at: delivery.created_at, now: now)</td>
                                    <td>(delivery.alert.headline())</td>
                                    <td class="hide-sm">(delivery.channel_name.as_str())</td>
                                    <td>
                                        match delivery.status {
                                            DeliveryStatus::Sent => <span class="pill pill-up">"Sent"</span>,
                                            DeliveryStatus::Pending => <span class="pill pill-pending">(if delivery.attempts == 0 { "Queued" } else { "Retrying" })</span>,
                                            DeliveryStatus::Failed => <span class="pill pill-down">"Failed"</span>,
                                        }
                                    </td>
                                    <td class="num hide-sm">(delivery.attempts)</td>
                                    <td class="small">(delivery.last_error.clone().unwrap_or_default())</td>
                                </tr>
                            }
                        </tbody>
                    </table>
                }
            </div>
        )
    })
}

#[component]
async fn channel_form(
    form: &ChannelForm,
    errors: &FormErrors,
    action: &str,
    editing: bool,
) -> Result<impl View> {
    let kinds = [
        (ChannelKind::Slack, "Slack (incoming webhook)"),
        (ChannelKind::Discord, "Discord (channel webhook)"),
        (
            ChannelKind::Webhook,
            "Webhook (JSON POST, optional HMAC signature)",
        ),
    ];
    let kind_signal = format!("'{}'", form.kind.replace('\'', ""));
    let secret_hint = if editing {
        "Leave blank to keep the current secret. Signs requests with X-Uptimestatus-Signature."
    } else {
        "Optional. Signs each request: X-Uptimestatus-Signature: sha256=HMAC(secret, timestamp.body)."
    };
    Ok(view! {
        <form method="post" action=(action) data-signals:kind=(kind_signal)>
            if !errors.is_empty() {
                <p class="flash flash-error" role="alert">"Some fields need attention."</p>
            }
            <fieldset>
                <legend>"Channel"</legend>
                <div class="fields">
                    text_field(name: "name", label: "Name", value: &form.name, errors: errors, placeholder: "Ops Slack")
                    <div class="field">
                        <label for="field-kind">"Kind"</label>
                        <select id="field-kind" name="kind" data-bind:kind="">
                            for (kind, label) in kinds {
                                <option value=(kind.as_str()) selected=(form.kind == kind.as_str())>(label)</option>
                            }
                        </select>
                        if let Some(error) = errors.get("kind") { <span class="error">(error.to_owned())</span> }
                    </div>
                </div>
                text_field(name: "url", label: "Webhook URL", value: &form.url, errors: errors,
                    placeholder: "https://hooks.slack.com/services/…",
                    hint: "Slack: Apps → Incoming Webhooks. Discord: Channel settings → Integrations → Webhooks.")
                <div data-show="$kind == 'webhook'">
                    text_field(name: "secret", label: "Signing secret", value: "", errors: errors, hint: secret_hint)
                </div>
                checkbox(name: "default_on", label: "Switch on for new monitors", checked: form.default_on.is_some())
            </fieldset>
            <fieldset>
                <legend>"Routing"</legend>
                <div class="fields">
                    text_field(name: "escalate_after", label: "Only alert after down for (minutes)", value: &form.escalate_after,
                        errors: errors, placeholder: "0",
                        hint: "Use for escalation: this channel hears about an outage only if it lasts this long.")
                    <div class="field">
                        <label for="field-quiet_start">"Quiet hours (UTC)"</label>
                        <div class="row">
                            <input type="time" id="field-quiet_start" name="quiet_start" value=(form.quiet_start.as_str())
                                aria-invalid=(errors.get("quiet_start").is_some().then_some("true"))>
                            "to"
                            <input type="time" id="field-quiet_end" name="quiet_end" value=(form.quiet_end.as_str())>
                        </div>
                        <span class="hint">"Alerts wait until the window ends. Blank means always on."</span>
                        if let Some(error) = errors.get("quiet_start") { <span class="error">(error.to_owned())</span> }
                    </div>
                </div>
                checkbox(name: "mute_recovered", label: "Don't send recoveries", checked: form.mute_recovered.is_some())
                checkbox(name: "mute_resend", label: "Don't send reminders while still down", checked: form.mute_resend.is_some())
                checkbox(name: "mute_cert", label: "Don't send certificate-expiry warnings", checked: form.mute_cert.is_some())
            </fieldset>
            <div class="actions">
                <button class="btn btn-primary" type="submit">"Save channel"</button>
                <a class="btn btn-link" href="/admin/alerts">"Cancel"</a>
            </div>
        </form>
    })
}

#[page("/admin/alerts/new")]
pub(crate) async fn new_channel(cx: &Cx) -> Result<impl View> {
    let admin = require_admin(cx).await?;
    let form = ChannelForm::new_channel();
    let errors = FormErrors::default();
    Ok(view! {
        admin_shell(title: "New alert channel", admin: &admin, section: "alerts",
            <div class="page-head"><h1>"New alert channel"</h1></div>
            <div class="card">channel_form(form: &form, errors: &errors, action: "/admin/alerts", editing: false)</div>
        )
    })
}

#[route(POST "/admin/alerts")]
pub(crate) async fn create_channel(cx: &Cx, Form(input): Form<ChannelForm>) -> Result<Response> {
    let admin = require_admin(cx).await?;
    match input.to_spec(None) {
        Ok(spec) => {
            let channel = app(cx)
                .store
                .create_channel(&spec, Timestamp::now())
                .await?;
            tracing::info!(channel = channel.id, kind = %spec.kind, by = %admin.login, "alert channel created");
            see_other("/admin/alerts").into_response(cx)
        }
        Err(errors) => {
            form_page(
                cx,
                &admin,
                "New alert channel",
                &input,
                &errors,
                "/admin/alerts",
                false,
            )
            .await
        }
    }
}

async fn load(cx: &Cx) -> Result<Channel> {
    Ok(app(cx)
        .store
        .channel(channel_id(cx)?)
        .await?
        .ok_or_else(not_found)?)
}

#[page("/admin/alerts/{id}")]
pub(crate) async fn edit_channel(cx: &Cx) -> Result<impl View> {
    let admin = require_admin(cx).await?;
    let channel = load(cx).await?;
    let form = ChannelForm::from_spec(&channel.spec);
    let errors = FormErrors::default();
    let action = format!("/admin/alerts/{}", channel.id);
    let delete_url = format!("{action}/delete");
    Ok(view! {
        admin_shell(title: &channel.spec.name, admin: &admin, section: "alerts",
            <div class="page-head">
                <h1>(channel.spec.name.as_str())</h1>
                <span class="spacer"></span>
                delete_button(action: &delete_url, heading: "Delete this channel?",
                    body: "Monitors stop alerting it, and its queued alerts are dropped.")
            </div>
            <div class="card">channel_form(form: &form, errors: &errors, action: &action, editing: true)</div>
        )
    })
}

#[route(POST "/admin/alerts/{id}")]
pub(crate) async fn update_channel(cx: &Cx, Form(input): Form<ChannelForm>) -> Result<Response> {
    let admin = require_admin(cx).await?;
    let channel = load(cx).await?;
    let action = format!("/admin/alerts/{}", channel.id);
    match input.to_spec(channel.spec.secret.clone()) {
        Ok(spec) => match app(cx).store.update_channel(channel.id, &spec).await {
            Ok(_) => {
                tracing::info!(channel = channel.id, by = %admin.login, "alert channel updated");
                see_other("/admin/alerts").into_response(cx)
            }
            Err(StoreError::ChannelNotFound(_)) => Err(not_found().into()),
            Err(error) => Err(error.into()),
        },
        Err(errors) => {
            form_page(
                cx,
                &admin,
                "Edit alert channel",
                &input,
                &errors,
                &action,
                true,
            )
            .await
        }
    }
}

#[route(POST "/admin/alerts/{id}/delete")]
pub(crate) async fn delete_channel(cx: &Cx) -> Result<SeeOther> {
    let admin = require_admin(cx).await?;
    let id = channel_id(cx)?;
    if !app(cx).store.delete_channel(id).await? {
        return Err(not_found().into());
    }
    tracing::info!(channel = id, by = %admin.login, "alert channel deleted");
    Ok(see_other("/admin/alerts"))
}

/// Sends a test alert now and patches the result into the channel's row.
#[route(POST "/admin/alerts/{id}/test")]
pub(crate) async fn test_channel(cx: &Cx) -> Result<PatchElements> {
    require_admin(cx).await?;
    let channel = load(cx).await?;
    let outcome = match &app(cx).sender {
        Some(sender) => sender
            .send(&channel.spec, &Alert::test(Timestamp::now()))
            .await
            .map_err(|e| e.message),
        None => Err("Sending is not available on this server.".to_owned()),
    };
    let id = format!("test-{}", channel.id);
    let html = view! { cx =>
        <td class="small" id=(id)>
            match &outcome {
                Ok(()) => <span class="pill pill-up">"Delivered"</span>,
                Err(message) => <span><span class="pill pill-down">"Failed"</span> " " (message.clone())</span>,
            }
        </td>
    }
    .single()
    .await?
    .render(cx);
    Ok(PatchElements::new(html))
}

async fn form_page(
    cx: &Cx,
    admin: &AdminUser,
    title: &str,
    input: &ChannelForm,
    errors: &FormErrors,
    action: &str,
    editing: bool,
) -> Result<Response> {
    let html = view! { cx =>
        admin_shell(title: title, admin: admin, section: "alerts",
            <div class="page-head"><h1>(title)</h1></div>
            <div class="card">channel_form(form: input, errors: errors, action: action, editing: editing)</div>
        )
    }
    .single()
    .await?
    .render(cx);
    (StatusCode::UNPROCESSABLE_ENTITY, Html(html)).into_response(cx)
}
