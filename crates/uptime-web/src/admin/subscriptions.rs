//! A status page's email: its sender (and sending domain at Lettermint),
//! whether visitors may subscribe, and the list of subscribers.

use std::collections::HashMap;

use jiff::Timestamp;
use serde::Deserialize;
use serde_json::json;
use topcoat::{
    Result,
    context::Cx,
    router::{
        content::Form,
        error::{SeeOther, not_found, see_other},
        header, page, path_param, query_params,
        response::{IntoResponse as _, Response},
        route,
    },
    view::{View, component, view},
};
use uptime_domain::{EmailAddress, SubscriberKind, SubscriberStatus};
use uptime_store::{EmailSettings, EmailSettingsInput, Page, Subscriber};

use super::{
    Id,
    views::{checkbox, text_field},
};
use crate::{
    AppState, audit,
    auth::{app, require_admin},
    views::{admin_shell, ago},
};

path_param!(sid: i64, error = not_found);

fn page_id(cx: &Cx) -> Result<i64> {
    Ok(*path_param::<Id>(cx)?)
}

async fn load_page(cx: &Cx) -> Result<Page> {
    Ok(app(cx)
        .store
        .page(page_id(cx)?)
        .await?
        .ok_or_else(not_found)?)
}

/// `?notice=` / `?error=` after an action.
#[query_params]
pub(crate) struct Flash {
    notice: Option<String>,
    error: Option<String>,
}

fn flash(cx: &Cx) -> (Option<String>, Option<String>) {
    query_params::<Flash>(cx)
        .ok()
        .map_or((None, None), |f| (f.notice.clone(), f.error.clone()))
}

fn back(page: i64, path: &str, key: &str, message: &str) -> SeeOther {
    let mut url = url::form_urlencoded::Serializer::new(String::new());
    url.append_pair(key, message);
    see_other(format!("/admin/pages/{page}/{path}?{}", url.finish()))
}

const NOT_CONFIGURED: &str = "Email is not configured. Set email.lettermint_project_token and email.default_from (and email.lettermint_team_token to register sending domains) to send alerts by email and let visitors subscribe.";

// ── Email settings ───────────────────────────────────────────────────────

#[page("/admin/pages/{id}/email")]
pub(crate) async fn email_page(cx: &Cx) -> Result<impl View> {
    let admin = require_admin(cx).await?;
    let page = load_page(cx).await?;
    let state = app(cx);
    let settings = state.store.email_settings(page.id).await?;
    let (notice, error) = flash(cx);
    let enabled = state.email_enabled();
    let manages = state
        .mailer()
        .is_some_and(uptime_notify::Mailer::manages_domains);
    let default_from = state
        .mailer()
        .map(|m| m.default_from().to_string())
        .unwrap_or_default();
    let title = format!("Email · {}", page.spec.title);
    let base = format!("/admin/pages/{}", page.id);
    let action = format!("{base}/email");
    Ok(view! {
        admin_shell(title: &title, admin: &admin, section: "pages",
            <div class="page-head">
                <h1>"Email" <span class="muted">" · " (page.spec.title.as_str())</span></h1>
                <span class="spacer"></span>
                <a class="btn" href=(format!("{base}/subscribers"))>"Subscribers"</a>
                <a class="btn" href=(base.clone())>"Back to the page"</a>
            </div>
            if let Some(notice) = &notice { <p class="flash" role="status">(notice.as_str())</p> }
            if let Some(error) = &error { <p class="flash flash-error" role="alert">(error.as_str())</p> }
            if !enabled {
                <p class="flash">(NOT_CONFIGURED)</p>
            }
            settings_form(settings: &settings, action: &action, default_from: &default_from)
            if enabled {
                sender_domain(settings: &settings, base: &base, manages: manages)
            }
        )
    })
}

#[component]
async fn settings_form(
    settings: &EmailSettings,
    action: &str,
    default_from: &str,
) -> Result<impl View> {
    let address =
        |a: &Option<EmailAddress>| a.as_ref().map(ToString::to_string).unwrap_or_default();
    let (sender, reply) = (
        address(&settings.sender_address),
        address(&settings.reply_to),
    );
    let name = settings.sender_name.clone().unwrap_or_default();
    let fallback = if default_from.is_empty() {
        "Until the sender's domain is verified, email goes from the default sender with the page's name.".to_owned()
    } else {
        format!(
            "Until the sender's domain is verified, email goes from {default_from}, named after the page, with this address as Reply-To."
        )
    };
    let no_errors = super::form::FormErrors::default();
    Ok(view! {
        <div class="card">
            <h2>"Subscriptions and sender"</h2>
            <form method="post" action=(action)>
                checkbox(name: "subscriptions", label: "Let visitors subscribe by email or webhook", checked: settings.subscriptions)
                checkbox(name: "notify_automatic", label: "Tell subscribers about automatic incidents (not only declared ones)", checked: settings.notify_automatic)
                <div class="fields">
                    text_field(name: "sender_name", label: "Sender name", value: &name, errors: &no_errors,
                        placeholder: "Acme Status", hint: "Defaults to the page's title.")
                    text_field(name: "sender_address", label: "Sender address", value: &sender, errors: &no_errors,
                        placeholder: "status@mail.example.com",
                        hint: "Best on a subdomain used only for sending, e.g. mail.example.com.")
                    text_field(name: "reply_to", label: "Reply-To", value: &reply, errors: &no_errors,
                        placeholder: "support@example.com", hint: "Optional: where replies go.")
                </div>
                <p class="small muted">(fallback)</p>
                <div class="actions"><button class="btn btn-primary" type="submit">"Save"</button></div>
            </form>
        </div>
    })
}

#[component]
async fn sender_domain(settings: &EmailSettings, base: &str, manages: bool) -> Result<impl View> {
    let address = settings.sender_address.as_ref().map(ToString::to_string);
    let domain = settings.domain.clone();
    let verified = settings.verified_sender().is_some();
    Ok(view! {
        <div class="card">
            <h2>"Sending domain"</h2>
            if let Some(address) = &address {
                if let Some(domain) = &domain {
                    <p>
                        <strong>(domain.name.as_str())</strong> " "
                        <span class=(if verified { "pill pill-up" } else { "pill pill-pending" })>(domain.status.label())</span>
                        if let Some(at) = domain.checked_at { " " <span class="small muted">"checked " ago(at: at, now: Timestamp::now())</span> }
                    </p>
                    if !domain.records.is_empty() {
                        <p class="small muted">"Add these records at your DNS provider (in Cloudflare, set CNAME records to DNS only), then verify."</p>
                        <div class="table-wrap">
                            <table class="list">
                                <thead><tr><th>"Type"</th><th>"Name"</th><th>"Value"</th><th>"Status"</th></tr></thead>
                                <tbody>
                                    for record in &domain.records {
                                        <tr>
                                            <td class="mono small">(record.kind.as_str())</td>
                                            <td class="mono small">(record.name.as_str()) " " <sb-copy-button value=(record.name.as_str()) label="Copy name"></sb-copy-button></td>
                                            <td class="mono small">(record.value.as_str()) " " <sb-copy-button value=(record.value.as_str()) label="Copy value"></sb-copy-button></td>
                                            <td class="small">(record.status.as_str()) (if record.required { "" } else { " (recommended)" })</td>
                                        </tr>
                                    }
                                </tbody>
                            </table>
                        </div>
                    }
                    if manages && domain.id != "manual" {
                        <form method="post" action=(format!("{base}/email/domain/verify"))>
                            <div class="actions"><button class="btn" type="submit">"Verify"</button></div>
                        </form>
                    }
                } else if manages {
                    <p>(format!("Register {} at Lettermint to send from {address}.", address.rsplit_once('@').map_or("", |(_, d)| d)))</p>
                    <form method="post" action=(format!("{base}/email/domain"))>
                        <div class="actions"><button class="btn btn-primary" type="submit">"Set up sending domain"</button></div>
                    </form>
                } else {
                    <p class="small">"Without a Lettermint team token, add the domain in the Lettermint dashboard (Domains → Add domain), publish its DNS records, and verify it there. Then send a test below: if it is accepted from this sender, the page sends from it."</p>
                }
            } else {
                <p class="muted">"Set a sender address above to send from the page's own domain."</p>
            }
            <form method="post" action=(format!("{base}/email/test"))>
                <div class="fields">
                    <div class="field">
                        <label for="field-test_to">"Send a test email to"</label>
                        <input id="field-test_to" type="email" name="to" required="" placeholder="you@example.com">
                    </div>
                </div>
                <div class="actions"><button class="btn" type="submit">"Send test"</button></div>
            </form>
        </div>
    })
}

/// The settings form as typed.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct EmailForm {
    subscriptions: Option<String>,
    notify_automatic: Option<String>,
    sender_name: String,
    sender_address: String,
    reply_to: String,
}

fn optional_address(text: &str, label: &str) -> Result<Option<EmailAddress>, String> {
    match text.trim() {
        "" => Ok(None),
        text => text
            .parse()
            .map(Some)
            .map_err(|_| format!("{label}: `{text}` is not a valid email address.")),
    }
}

fn settings_json(settings: &EmailSettings) -> serde_json::Value {
    json!({
        "subscriptions": settings.subscriptions,
        "notify_automatic": settings.notify_automatic,
        "sender_name": settings.sender_name,
        "sender_address": settings.sender_address,
        "reply_to": settings.reply_to,
    })
}

#[route(POST "/admin/pages/{id}/email")]
pub(crate) async fn save_email(cx: &Cx, Form(input): Form<EmailForm>) -> Result<SeeOther> {
    let admin = require_admin(cx).await?;
    let page = load_page(cx).await?;
    let state = app(cx);
    let (sender, reply_to) = match (
        optional_address(&input.sender_address, "Sender address"),
        optional_address(&input.reply_to, "Reply-To"),
    ) {
        (Ok(sender), Ok(reply)) => (sender, reply),
        (Err(error), _) | (_, Err(error)) => return Ok(back(page.id, "email", "error", &error)),
    };
    let before = state.store.email_settings(page.id).await?;
    let after = state
        .store
        .save_email_settings(
            page.id,
            &EmailSettingsInput {
                subscriptions: input.subscriptions.is_some(),
                notify_automatic: input.notify_automatic.is_some(),
                sender_name: Some(input.sender_name.clone()),
                sender_address: sender,
                reply_to,
            },
        )
        .await?;
    state.pages.clear();
    let details = audit::plain_changes(&settings_json(&before), &settings_json(&after));
    audit::record(
        state,
        audit::admin(&admin),
        "page.email",
        audit::target("page", page.id),
        format!(
            "Updated email settings of {} ({})",
            page.spec.title,
            audit::changed_fields(&details).join(", ")
        ),
        details,
    )
    .await?;
    Ok(back(page.id, "email", "notice", "Saved."))
}

#[route(POST "/admin/pages/{id}/email/domain")]
pub(crate) async fn setup_domain(cx: &Cx) -> Result<SeeOther> {
    let admin = require_admin(cx).await?;
    let page = load_page(cx).await?;
    let state = app(cx);
    let Some(mailer) = state.mailer().filter(|m| m.manages_domains()) else {
        return Ok(back(
            page.id,
            "email",
            "error",
            "Registering domains needs email.lettermint_team_token.",
        ));
    };
    let settings = state.store.email_settings(page.id).await?;
    let Some(address) = settings.sender_address else {
        return Ok(back(
            page.id,
            "email",
            "error",
            "Set a sender address first.",
        ));
    };
    match mailer.register_domain(address.domain()).await {
        Ok(domain) => {
            state
                .store
                .set_sender_domain(page.id, Some(&domain))
                .await?;
            audit::record(
                state,
                audit::admin(&admin),
                "page.sender_domain",
                audit::target("page", page.id),
                format!(
                    "Registered sending domain {} for {}",
                    domain.name, page.spec.title
                ),
                json!({ "domain": domain.name, "status": domain.status.as_str() }),
            )
            .await?;
            Ok(back(
                page.id,
                "email",
                "notice",
                "Domain registered: add its DNS records, then verify.",
            ))
        }
        Err(error) => Ok(back(page.id, "email", "error", &error.to_string())),
    }
}

#[route(POST "/admin/pages/{id}/email/domain/verify")]
pub(crate) async fn verify_domain(cx: &Cx) -> Result<SeeOther> {
    let admin = require_admin(cx).await?;
    let page = load_page(cx).await?;
    let state = app(cx);
    let settings = state.store.email_settings(page.id).await?;
    let (Some(mailer), Some(domain)) = (
        state.mailer().filter(|m| m.manages_domains()),
        settings.domain,
    ) else {
        return Ok(back(
            page.id,
            "email",
            "error",
            "Set up the sending domain first.",
        ));
    };
    match mailer.verify_domain(&domain.id).await {
        Ok(checked) => {
            state
                .store
                .set_sender_domain(page.id, Some(&checked))
                .await?;
            audit::record(
                state,
                audit::admin(&admin),
                "page.sender_domain",
                audit::target("page", page.id),
                format!(
                    "Checked sending domain {}: {}",
                    checked.name,
                    checked.status.label()
                ),
                json!({ "domain": checked.name, "status": checked.status.as_str() }),
            )
            .await?;
            Ok(back(page.id, "email", "notice", checked.status.label()))
        }
        Err(error) => Ok(back(page.id, "email", "error", &error.to_string())),
    }
}

/// `to=` for a test email.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct TestForm {
    to: String,
}

#[route(POST "/admin/pages/{id}/email/test")]
pub(crate) async fn send_test(cx: &Cx, Form(input): Form<TestForm>) -> Result<SeeOther> {
    let admin = require_admin(cx).await?;
    let page = load_page(cx).await?;
    let state = app(cx);
    let Ok(to) = input.to.parse::<EmailAddress>() else {
        return Ok(back(
            page.id,
            "email",
            "error",
            "Enter a valid address to send the test to.",
        ));
    };
    let Some(mailer) = state.mailer() else {
        return Ok(back(page.id, "email", "error", NOT_CONFIGURED));
    };
    let Some(mut mail) = uptime_runtime::subscriptions::page_mail(
        &state.store,
        Some(mailer),
        state.hosts.app_url(),
        page.id,
    )
    .await?
    else {
        return Err(not_found().into());
    };
    let settings = state.store.email_settings(page.id).await?;
    // Without the Team API, trying the page's own sender is how we learn
    // whether its domain is verified at Lettermint.
    let trying_own = !mailer.manages_domains()
        && settings.verified_sender().is_none()
        && settings.sender_address.is_some();
    if let (true, Some(own)) = (trying_own, &settings.sender_address) {
        mail.from = uptime_domain::Mailbox::new(
            Some(settings.sender_name.as_deref().unwrap_or(&page.spec.title)),
            own.clone(),
        );
    }
    let letter = uptime_notify::letters::test_letter(&mail.brand);
    let sent = mailer
        .send(&uptime_notify::Email {
            from: mail.from.clone(),
            to: vec![to.clone()],
            reply_to: mail.reply_to.clone(),
            subject: letter.subject,
            html: letter.html,
            text: letter.text,
            headers: Vec::new(),
            idempotency_key: format!("test-{}-{}", page.id, Timestamp::now().as_nanosecond()),
            tag: "test".into(),
        })
        .await;
    audit::record(
        state,
        audit::admin(&admin),
        "page.email_test",
        audit::target("page", page.id),
        format!(
            "Sent a test email for {} to {}",
            page.spec.title,
            to.masked()
        ),
        json!({ "from": mail.from.address.to_string(), "ok": sent.is_ok() }),
    )
    .await?;
    match sent {
        Ok(()) => {
            if trying_own && let Some(own) = &settings.sender_address {
                let domain = uptime_domain::SenderDomain {
                    id: "manual".into(),
                    name: own.domain().to_owned(),
                    status: uptime_domain::DomainState::Verified,
                    records: Vec::new(),
                    checked_at: Some(Timestamp::now()),
                };
                state
                    .store
                    .set_sender_domain(page.id, Some(&domain))
                    .await?;
                return Ok(back(
                    page.id,
                    "email",
                    "notice",
                    &format!("Test accepted from {own}: the page now sends from it."),
                ));
            }
            Ok(back(
                page.id,
                "email",
                "notice",
                &format!("Test sent from {}.", mail.from),
            ))
        }
        Err(error) => Ok(back(
            page.id,
            "email",
            "error",
            &format!("The test was not accepted: {}", error.message),
        )),
    }
}

// ── Subscribers ──────────────────────────────────────────────────────────

/// "All", or the followed components' names.
fn following(subscriber: &Subscriber, names: &HashMap<String, String>) -> String {
    if subscriber.selection.is_all() {
        return "All components".to_owned();
    }
    subscriber
        .selection
        .0
        .iter()
        .map(|key| names.get(key).cloned().unwrap_or_else(|| key.clone()))
        .collect::<Vec<_>>()
        .join(", ")
}

fn contact(subscriber: &Subscriber) -> String {
    match (&subscriber.kind, &subscriber.webhook_url) {
        (SubscriberKind::Webhook, Some(url)) => format!(
            "{} (owner {})",
            url.host_str().unwrap_or_default(),
            subscriber.email.masked()
        ),
        _ => subscriber.email.masked(),
    }
}

fn status_pill(status: SubscriberStatus) -> &'static str {
    match status {
        SubscriberStatus::Active => "pill pill-up",
        SubscriberStatus::Pending => "pill pill-pending",
        SubscriberStatus::Unsubscribed => "pill pill-paused",
        SubscriberStatus::Suppressed | SubscriberStatus::Disabled => "pill pill-down",
    }
}

async fn component_names(state: &AppState, page: &Page) -> Result<HashMap<String, String>> {
    let monitors: HashMap<String, String> = state
        .store
        .list_monitors()
        .await?
        .into_iter()
        .map(|m| (m.spec.key.to_string(), m.spec.name))
        .collect();
    Ok(page
        .spec
        .sections
        .iter()
        .flat_map(|s| &s.components)
        .map(|c| {
            let key = c.key().to_string();
            let name = c
                .label()
                .map(str::to_owned)
                .or_else(|| monitors.get(&key).cloned())
                .unwrap_or_else(|| key.clone());
            (key, name)
        })
        .collect())
}

#[page("/admin/pages/{id}/subscribers")]
pub(crate) async fn subscribers_page(cx: &Cx) -> Result<impl View> {
    let admin = require_admin(cx).await?;
    let page = load_page(cx).await?;
    let state = app(cx);
    let subscribers = state.store.list_subscribers(page.id).await?;
    let counts = state.store.subscriber_counts(page.id).await?;
    let names = component_names(state, &page).await?;
    let rows: Vec<(Subscriber, String, String)> = subscribers
        .into_iter()
        .map(|s| {
            let follows = following(&s, &names);
            let who = contact(&s);
            (s, who, follows)
        })
        .collect();
    let (notice, _) = flash(cx);
    let enabled = state.email_enabled();
    let now = Timestamp::now();
    let title = format!("Subscribers · {}", page.spec.title);
    let base = format!("/admin/pages/{}", page.id);
    Ok(view! {
        admin_shell(title: &title, admin: &admin, section: "pages",
            <div class="page-head">
                <h1>"Subscribers" <span class="muted">" · " (page.spec.title.as_str())</span></h1>
                <span class="spacer"></span>
                <a class="btn" href=(format!("{base}/subscribers.csv"))>"Export CSV"</a>
                <a class="btn" href=(format!("{base}/email"))>"Email settings"</a>
                <a class="btn" href=(base.clone())>"Back to the page"</a>
            </div>
            if let Some(notice) = &notice { <p class="flash" role="status">(notice.as_str())</p> }
            if !enabled { <p class="flash">(NOT_CONFIGURED)</p> }
            <dl class="windows">
                <div><dt>"Active"</dt><dd>(counts.active)</dd></div>
                <div><dt>"Awaiting confirmation"</dt><dd>(counts.pending)</dd></div>
                <div><dt>"Inactive"</dt><dd>(counts.inactive)</dd></div>
            </dl>
            <div class="card table-wrap">
                if rows.is_empty() {
                    <p class="muted">"No subscribers yet."</p>
                } else {
                    <table class="list">
                        <thead><tr><th>"Subscriber"</th><th>"Kind"</th><th>"Status"</th><th class="hide-sm">"Follows"</th><th class="hide-sm">"Since"</th><th class="hide-sm">"Last delivery"</th><th></th></tr></thead>
                        <tbody>
                            for (subscriber, who, follows) in &rows {
                                <tr>
                                    <td class="mono small">(who.as_str())</td>
                                    <td>(subscriber.kind.label())</td>
                                    <td><span class=(status_pill(subscriber.status))>(subscriber.status.label())</span></td>
                                    <td class="small hide-sm">(follows.as_str())</td>
                                    <td class="small hide-sm">ago(at: subscriber.created_at, now: now)</td>
                                    <td class="small hide-sm">
                                        if let Some(error) = &subscriber.last_error {
                                            <span class="error-text">(error.as_str())</span>
                                        } else if let Some(at) = subscriber.last_delivery_at {
                                            ago(at: at, now: now)
                                        } else { "—" }
                                    </td>
                                    <td class="row-actions">
                                        if matches!(subscriber.status, SubscriberStatus::Active | SubscriberStatus::Pending) {
                                            <form method="post" action=(format!("{base}/subscribers/{}/stop", subscriber.id))>
                                                <button class="btn btn-link" type="submit">(if subscriber.kind == SubscriberKind::Webhook { "Disable" } else { "Unsubscribe" })</button>
                                            </form>
                                        }
                                        <form method="post" action=(format!("{base}/subscribers/{}/remove", subscriber.id))>
                                            <button class="btn btn-link" type="submit">"Remove"</button>
                                        </form>
                                    </td>
                                </tr>
                            }
                        </tbody>
                    </table>
                }
            </div>
        )
    })
}

async fn page_subscriber(cx: &Cx) -> Result<(Page, Subscriber)> {
    let page = load_page(cx).await?;
    let id = *path_param::<Sid>(cx)?;
    let subscriber = app(cx)
        .store
        .subscriber(id)
        .await?
        .filter(|s| s.page_id == page.id)
        .ok_or_else(not_found)?;
    Ok((page, subscriber))
}

#[route(POST "/admin/pages/{id}/subscribers/{sid}/stop")]
pub(crate) async fn stop_subscriber(cx: &Cx) -> Result<SeeOther> {
    let admin = require_admin(cx).await?;
    let (page, subscriber) = page_subscriber(cx).await?;
    let state = app(cx);
    state
        .store
        .unsubscribe(subscriber.id, Timestamp::now())
        .await?;
    audit::record(
        state,
        audit::admin(&admin),
        "subscriber.stop",
        audit::target("page", page.id),
        format!(
            "Stopped sending to {} on {}",
            contact(&subscriber),
            page.spec.title
        ),
        json!({ "subscriber": subscriber.id, "kind": subscriber.kind.as_str() }),
    )
    .await?;
    Ok(back(page.id, "subscribers", "notice", "Stopped."))
}

#[route(POST "/admin/pages/{id}/subscribers/{sid}/remove")]
pub(crate) async fn remove_subscriber(cx: &Cx) -> Result<SeeOther> {
    let admin = require_admin(cx).await?;
    let (page, subscriber) = page_subscriber(cx).await?;
    let state = app(cx);
    state.store.remove_subscriber(subscriber.id).await?;
    audit::record(
        state,
        audit::admin(&admin),
        "subscriber.remove",
        audit::target("page", page.id),
        format!(
            "Removed subscriber {} from {}",
            contact(&subscriber),
            page.spec.title
        ),
        json!({ "subscriber": subscriber.id, "kind": subscriber.kind.as_str() }),
    )
    .await?;
    Ok(back(page.id, "subscribers", "notice", "Removed."))
}

/// Quotes a CSV field.
fn csv(field: &str) -> String {
    // Leading formula characters are neutralised for spreadsheet apps.
    let field = if field.starts_with(['=', '+', '-', '@']) {
        format!("'{field}")
    } else {
        field.to_owned()
    };
    format!("\"{}\"", field.replace('"', "\"\""))
}

#[route(GET "/admin/pages/{id}/subscribers.csv")]
pub(crate) async fn export_subscribers(cx: &Cx) -> Result<Response> {
    let admin = require_admin(cx).await?;
    let page = load_page(cx).await?;
    let state = app(cx);
    let subscribers = state.store.list_subscribers(page.id).await?;
    let names = component_names(state, &page).await?;
    let mut out = String::from("kind,email,webhook_host,status,follows,created_at,confirmed_at\n");
    for s in &subscribers {
        let row = [
            s.kind.as_str().to_owned(),
            s.email.to_string(),
            s.webhook_url
                .as_ref()
                .and_then(|u| u.host_str())
                .unwrap_or_default()
                .to_owned(),
            s.status.as_str().to_owned(),
            following(s, &names),
            s.created_at.to_string(),
            s.confirmed_at.map(|t| t.to_string()).unwrap_or_default(),
        ];
        out.push_str(&row.iter().map(|f| csv(f)).collect::<Vec<_>>().join(","));
        out.push('\n');
    }
    audit::record(
        state,
        audit::admin(&admin),
        "subscriber.export",
        audit::target("page", page.id),
        format!(
            "Exported {} subscribers of {}",
            subscribers.len(),
            page.spec.title
        ),
        json!({ "count": subscribers.len() }),
    )
    .await?;
    let file = format!(
        "attachment; filename=\"{}-subscribers.csv\"",
        page.spec.slug
    );
    (
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_owned()),
            (header::CONTENT_DISPOSITION, file),
        ],
        out,
    )
        .into_response(cx)
}
