//! Subscribing to a status page: by email, or a webhook confirmed by email.
//!
//! Only when email is configured and the page allows subscriptions. Double
//! opt-in: nothing but the confirmation is sent until it is confirmed.
//! Visitors are never told whether an address is already subscribed: every
//! accepted request answers "check your inbox". Requests are rate-limited per
//! client and per address, and a hidden field catches simple bots.
//!
//! Links in emails carry a signed token (`?token=`): confirming and
//! unsubscribing take a POST (a button on the page, or a mail client's
//! one-click `List-Unsubscribe-Post`), so link scanners cannot act on them.

use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

use jiff::Timestamp;
use serde::Deserialize;
use topcoat::{
    Result,
    context::Cx,
    router::{
        StatusCode,
        content::{Form, Html},
        error::{SeeOther, not_found, see_other},
        page, path_param, query_params,
        request::headers,
        response::{IntoResponse as _, Response},
        route,
    },
    view::{Unescaped, View, ViewExt as _, component, view},
};
use uptime_domain::{EmailAddress, Selection, SubscriberKind, SubscriberStatus};
use uptime_store::{LinkPurpose, MANAGE_FOR, NewSubscription, Subscriber};
use url::Url;

use super::{Slug, StatusView, Tab, base, frame, load};
use crate::auth::app;

/// Requests per client address per hour.
const PER_CLIENT: usize = 10;
/// Requests per subscriber address (or webhook) per day.
const PER_ADDRESS: usize = 3;

/// In-memory sliding-window rate limits (per instance).
#[derive(Clone, Default)]
pub(crate) struct Limits {
    hits: Arc<Mutex<HashMap<String, Vec<Instant>>>>,
}

impl std::fmt::Debug for Limits {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Limits")
    }
}

impl Limits {
    /// Records a hit for `key`; false when it is over `max` within `window`.
    fn allow(&self, key: &str, max: usize, window: Duration) -> bool {
        let now = Instant::now();
        let mut hits = self.hits.lock().unwrap_or_else(PoisonError::into_inner);
        if hits.len() > 10_000 {
            hits.retain(|_, times| times.iter().any(|t| now.duration_since(*t) < window));
        }
        let times = hits.entry(key.to_owned()).or_default();
        times.retain(|t| now.duration_since(*t) < window);
        if times.len() >= max {
            return false;
        }
        times.push(now);
        true
    }
}

/// A component visitors can follow: its key and label.
struct Choice {
    key: String,
    name: String,
}

/// The page, if visitors may subscribe to it now.
async fn subscribable(cx: &Cx) -> Result<(Arc<StatusView>, Vec<Choice>)> {
    let view = load(cx, path_param::<Slug>(cx)).await?;
    let state = app(cx);
    if !state.email_enabled() || !view.subscriptions {
        return Err(not_found().into());
    }
    let page = state
        .store
        .page_by_slug(&view.slug)
        .await?
        .ok_or_else(not_found)?;
    let monitors: HashMap<String, String> = state
        .store
        .list_monitors()
        .await?
        .into_iter()
        .map(|m| (m.spec.key.to_string(), m.spec.name))
        .collect();
    let mut choices: Vec<Choice> = Vec::new();
    for component in page.spec.sections.iter().flat_map(|s| &s.components) {
        let key = component.key().to_string();
        if choices.iter().any(|c| c.key == key) {
            continue;
        }
        let name = component
            .label()
            .map(str::to_owned)
            .or_else(|| monitors.get(&key).cloned())
            .unwrap_or_else(|| key.clone());
        choices.push(Choice { key, name });
    }
    Ok((view, choices))
}

/// The form as typed. `component_<key>=on` picks components; `website` is
/// the honeypot (people never see it).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct SubscribeForm {
    email: String,
    url: String,
    /// `all`, or `some` with `component_*` fields.
    scope: String,
    website: String,
    #[serde(flatten)]
    extra: HashMap<String, String>,
}

impl SubscribeForm {
    fn selection(&self, choices: &[Choice]) -> Selection {
        if self.scope != "some" {
            return Selection::all();
        }
        let picked: Vec<String> = choices
            .iter()
            .filter(|c| {
                self.extra
                    .get(&format!("component_{}", c.key))
                    .is_some_and(|v| v == "on")
            })
            .map(|c| c.key.clone())
            .collect();
        if picked.is_empty() || picked.len() == choices.len() {
            Selection::all()
        } else {
            Selection(picked)
        }
    }
}

/// Component checkboxes.
#[component]
async fn component_choices(choices: &[Choice], selection: &Selection) -> Result<impl View> {
    let all = selection.is_all();
    let rows: Vec<(String, String, bool)> = choices
        .iter()
        .map(|c| {
            (
                format!("component_{}", c.key),
                c.name.clone(),
                !all && selection.contains(&c.key),
            )
        })
        .collect();
    Ok(view! {
        <fieldset class="subscribe-components">
            <legend>"Components"</legend>
            <label class="choice"><input type="radio" name="scope" value="all" checked=(all.then_some(""))> " Everything on this page"</label>
            <label class="choice"><input type="radio" name="scope" value="some" checked=((!all).then_some(""))> " Only these:"</label>
            for (name, label, checked) in &rows {
                <label class="choice choice-nested"><input type="checkbox" name=(name.as_str()) checked=(checked.then_some(""))> " " (label.as_str())</label>
            }
        </fieldset>
    })
}

/// Explains what a subscriber will receive.
const WHAT_YOU_GET: &str = "Incidents as they are posted and resolved, and scheduled maintenance. Unsubscribe from any email.";

#[page("/s/{slug}/subscribe")]
pub(crate) async fn subscribe_page(cx: &Cx) -> Result<impl View> {
    let (view, choices) = subscribable(cx).await?;
    let base = base(cx, &view.slug);
    let title = format!("Subscribe · {}", view.title);
    Ok(view! {
        frame(view: &view, base: &base, tab: Tab::Subscribe, title: &title,
            subscribe_forms(view: &view, base: &base, choices: &choices, error: None, email: "", url: "")
        )
    })
}

#[component]
async fn subscribe_forms(
    view: &StatusView,
    base: &str,
    choices: &[Choice],
    /// `(form, message)`: `email` or `webhook`.
    error: Option<(&str, &str)>,
    email: &str,
    url: &str,
) -> Result<impl View> {
    let all = Selection::all();
    let email_error = error
        .filter(|(form, _)| *form == "email")
        .map(|(_, m)| m.to_owned());
    let webhook_error = error
        .filter(|(form, _)| *form == "webhook")
        .map(|(_, m)| m.to_owned());
    Ok(view! {
        <section class="subscribe">
            <article class="subscribe-card">
                <h2>"Get updates by email"</h2>
                <p class="muted">(WHAT_YOU_GET)</p>
                <form method="post" action=(format!("{base}/subscribe"))>
                    if let Some(error) = &email_error {
                        <p class="flash flash-error" role="alert">(error.as_str())</p>
                    }
                    <div class="field">
                        <label for="subscribe-email">"Email address"</label>
                        <input id="subscribe-email" type="email" name="email" required="" autocomplete="email" value=(email)>
                    </div>
                    <div class="hp" aria-hidden="true">
                        <label for="subscribe-website">"Website"</label>
                        <input id="subscribe-website" type="text" name="website" tabindex="-1" autocomplete="off">
                    </div>
                    component_choices(choices: choices, selection: &all)
                    <div class="actions"><button class="btn btn-primary" type="submit">"Subscribe"</button></div>
                </form>
            </article>
            <article class="subscribe-card">
                <h2>"Get updates by webhook"</h2>
                <p class="muted">"We POST a signed JSON event to your endpoint for every update. We confirm the webhook by email and tell you there if it keeps failing."</p>
                <form method="post" action=(format!("{base}/subscribe/webhook"))>
                    if let Some(error) = &webhook_error {
                        <p class="flash flash-error" role="alert">(error.as_str())</p>
                    }
                    <div class="field">
                        <label for="subscribe-url">"Webhook URL"</label>
                        <input id="subscribe-url" type="url" name="url" required="" placeholder="https://example.com/hooks/status" value=(url)>
                    </div>
                    <div class="field">
                        <label for="subscribe-owner">"Your email address"</label>
                        <input id="subscribe-owner" type="email" name="email" required="" autocomplete="email" value=(email)>
                    </div>
                    <div class="hp" aria-hidden="true">
                        <input type="text" name="website" tabindex="-1" autocomplete="off">
                    </div>
                    component_choices(choices: choices, selection: &all)
                    <div class="actions"><button class="btn" type="submit">"Add webhook"</button></div>
                </form>
            </article>
            <p class="small muted">(format!("Subscribing to {}. We only use your address for these updates.", view.title))</p>
        </section>
    })
}

/// The client's address, for rate limiting.
fn client(cx: &Cx) -> String {
    crate::logging::client_ip(headers(cx)).unwrap_or_else(|| "unknown".to_owned())
}

/// Renders `body` (HTML from `view!`) in the status frame with `status`.
async fn respond(
    cx: &Cx,
    page: &StatusView,
    status: StatusCode,
    title: &str,
    body: String,
) -> Result<Response> {
    let base = base(cx, &page.slug);
    let html = view! { cx =>
        frame(view: page, base: &base, tab: Tab::Subscribe, title: title,
            (Unescaped::new_unchecked(body.clone())))
    }
    .single()
    .await?
    .render(cx);
    (status, Html(html)).into_response(cx)
}

/// The same answer for every accepted request.
async fn check_inbox(cx: &Cx, view: &StatusView, webhook: bool) -> Result<Response> {
    let what = if webhook {
        "To finish adding the webhook, confirm it from the email we just sent."
    } else {
        "To finish subscribing, confirm from the email we just sent."
    };
    respond(
        cx,
        view,
        StatusCode::OK,
        "Check your inbox",
        view! { cx =>
            <section class="subscribe-done">
                <h2>"Check your inbox"</h2>
                <p>(what)</p>
                <p class="muted small">"If nothing arrives within a few minutes, check your spam folder or try again later."</p>
            </section>
        }
        .single()
        .await?
        .render(cx),
    )
    .await
}

async fn form_error(
    cx: &Cx,
    view: &StatusView,
    choices: &[Choice],
    form: &str,
    message: &str,
    input: &SubscribeForm,
) -> Result<Response> {
    let base = base(cx, &view.slug);
    respond(
        cx,
        view,
        StatusCode::UNPROCESSABLE_ENTITY,
        "Subscribe",
        view! { cx =>
            subscribe_forms(view: view, base: &base, choices: choices,
                error: Some((form, message)), email: &input.email, url: &input.url)
        }
        .single()
        .await?
        .render(cx),
    )
    .await
}

#[route(POST "/s/{slug}/subscribe")]
pub(crate) async fn subscribe_email(cx: &Cx, Form(input): Form<SubscribeForm>) -> Result<Response> {
    let (view, choices) = subscribable(cx).await?;
    if !input.website.is_empty() {
        return check_inbox(cx, &view, false).await;
    }
    let Ok(email) = input.email.parse::<EmailAddress>() else {
        return form_error(
            cx,
            &view,
            &choices,
            "email",
            "Enter a valid email address.",
            &input,
        )
        .await;
    };
    let state = app(cx);
    let limits = &state.subscribe_limits;
    let client_ok = limits.allow(&format!("client:{}", client(cx)), PER_CLIENT, HOUR);
    let address_ok = limits.allow(
        &format!("address:{}:{}", view.page_id, email.normalized()),
        PER_ADDRESS,
        DAY,
    );
    if client_ok && address_ok {
        state
            .store
            .subscribe(
                view.page_id,
                &NewSubscription::Email {
                    email,
                    selection: input.selection(&choices),
                },
                Timestamp::now(),
            )
            .await?;
    } else {
        tracing::warn!(page = %view.slug, "subscribe request rate-limited");
    }
    check_inbox(cx, &view, false).await
}

const HOUR: Duration = Duration::from_secs(60 * 60);
const DAY: Duration = Duration::from_secs(24 * 60 * 60);

/// A webhook URL visitors may register: https, a public host.
fn webhook_url(cx: &Cx, text: &str) -> Option<Url> {
    let url: Url = text.trim().parse().ok()?;
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    match url.host()? {
        url::Host::Domain(name) => name.contains('.').then_some(()),
        url::Host::Ipv4(v4) => permits(cx, IpAddr::V4(v4)).then_some(()),
        url::Host::Ipv6(v6) => permits(cx, IpAddr::V6(v6)).then_some(()),
    }?;
    Some(url)
}

fn permits(cx: &Cx, ip: IpAddr) -> bool {
    app(cx).prober.as_ref().is_some_and(|p| p.permits(ip))
}

#[route(POST "/s/{slug}/subscribe/webhook")]
pub(crate) async fn subscribe_webhook(
    cx: &Cx,
    Form(input): Form<SubscribeForm>,
) -> Result<Response> {
    let (view, choices) = subscribable(cx).await?;
    if !input.website.is_empty() {
        return check_inbox(cx, &view, true).await;
    }
    let Some(url) = webhook_url(cx, &input.url) else {
        return form_error(
            cx,
            &view,
            &choices,
            "webhook",
            "Enter a public https:// URL for the webhook.",
            &input,
        )
        .await;
    };
    let Ok(email) = input.email.parse::<EmailAddress>() else {
        return form_error(
            cx,
            &view,
            &choices,
            "webhook",
            "Enter a valid email address to confirm the webhook.",
            &input,
        )
        .await;
    };
    let state = app(cx);
    let limits = &state.subscribe_limits;
    let client_ok = limits.allow(&format!("client:{}", client(cx)), PER_CLIENT, HOUR);
    let address_ok =
        limits.allow(
            &format!("address:{}:{}", view.page_id, email.normalized()),
            PER_ADDRESS,
            DAY,
        ) && limits.allow(&format!("webhook:{}:{url}", view.page_id), PER_ADDRESS, DAY);
    if client_ok && address_ok {
        state
            .store
            .subscribe(
                view.page_id,
                &NewSubscription::Webhook {
                    url,
                    email,
                    selection: input.selection(&choices),
                },
                Timestamp::now(),
            )
            .await?;
    } else {
        tracing::warn!(page = %view.slug, "webhook subscribe request rate-limited");
    }
    check_inbox(cx, &view, true).await
}

/// `?token=`.
#[query_params]
pub(crate) struct TokenQuery {
    token: Option<String>,
}

/// A form carrying the token (and, for managing, the component choice).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct TokenForm {
    token: String,
    scope: String,
    #[serde(flatten)]
    extra: HashMap<String, String>,
}

fn query_token(cx: &Cx) -> Option<String> {
    query_params::<TokenQuery>(cx)
        .ok()
        .and_then(|q| q.token.clone())
        .filter(|t| !t.is_empty())
}

/// The subscriber a token opens on this page.
async fn open(
    cx: &Cx,
    view: &StatusView,
    token: Option<&str>,
    purpose: LinkPurpose,
) -> Result<Option<Subscriber>> {
    let Some(token) = token else {
        return Ok(None);
    };
    let found = app(cx)
        .store
        .open_subscriber_token(token, purpose, Timestamp::now())
        .await?;
    Ok(found.filter(|s| s.page_id == view.page_id))
}

async fn invalid_link(cx: &Cx, view: &StatusView) -> Result<Response> {
    let base = base(cx, &view.slug);
    respond(
        cx,
        view,
        StatusCode::NOT_FOUND,
        "Link expired",
        view! { cx =>
            <section class="subscribe-done">
                <h2>"This link has expired"</h2>
                <p>"It may be old, or the subscription was changed since. "
                    <a href=(format!("{base}/subscribe"))>"Subscribe again"</a> "."</p>
            </section>
        }
        .single()
        .await?
        .render(cx),
    )
    .await
}

/// The confirmation link: a button, so mail scanners cannot confirm.
#[route(GET "/s/{slug}/subscriptions/confirm")]
pub(crate) async fn confirm_page(cx: &Cx) -> Result<Response> {
    let view = load(cx, path_param::<Slug>(cx)).await?;
    let token = query_token(cx);
    if open(cx, &view, token.as_deref(), LinkPurpose::Confirm)
        .await?
        .is_none()
    {
        return invalid_link(cx, &view).await;
    }
    let base = base(cx, &view.slug);
    let token = token.unwrap_or_default();
    respond(
        cx,
        &view,
        StatusCode::OK,
        "Confirm subscription",
        view! { cx =>
            <section class="subscribe-done">
                <h2>"Confirm your subscription"</h2>
                <form method="post" action=(format!("{base}/subscriptions/confirm"))>
                    <input type="hidden" name="token" value=(token.as_str())>
                    <button class="btn btn-primary" type="submit">"Confirm subscription"</button>
                </form>
            </section>
        }
        .single()
        .await?
        .render(cx),
    )
    .await
}

#[route(POST "/s/{slug}/subscriptions/confirm")]
pub(crate) async fn confirm(cx: &Cx, Form(input): Form<TokenForm>) -> Result<Response> {
    let view = load(cx, path_param::<Slug>(cx)).await?;
    let Some(subscriber) = open(cx, &view, Some(&input.token), LinkPurpose::Confirm).await? else {
        return invalid_link(cx, &view).await;
    };
    let store = &app(cx).store;
    let now = Timestamp::now();
    if store
        .confirm_subscriber(subscriber.id, now)
        .await?
        .is_none()
    {
        return invalid_link(cx, &view).await;
    }
    let manage = store
        .subscriber_token(subscriber.id, LinkPurpose::Manage, now + MANAGE_FOR)
        .await?;
    let base = base(cx, &view.slug);
    see_other(manage_href(&base, &manage, "confirmed")).into_response(cx)
}

fn manage_href(base: &str, token: &str, flag: &str) -> String {
    let mut href = format!("{base}/subscriptions/manage?token={token}");
    if !flag.is_empty() {
        href.push_str(&format!("&{flag}=1"));
    }
    href
}

/// `?confirmed=1` / `?saved=1` after an action.
#[query_params]
pub(crate) struct ManageQuery {
    confirmed: Option<String>,
    saved: Option<String>,
    rotated: Option<String>,
}

#[route(GET "/s/{slug}/subscriptions/manage")]
pub(crate) async fn manage_page(cx: &Cx) -> Result<Response> {
    let view = load(cx, path_param::<Slug>(cx)).await?;
    let token = query_token(cx);
    let Some(subscriber) = open(cx, &view, token.as_deref(), LinkPurpose::Manage).await? else {
        return invalid_link(cx, &view).await;
    };
    let state = app(cx);
    let flags = query_params::<ManageQuery>(cx).ok();
    let notice = flags.as_ref().and_then(|q| {
        if q.confirmed.is_some() {
            Some("Subscription confirmed.")
        } else if q.saved.is_some() {
            Some("Saved.")
        } else if q.rotated.is_some() {
            Some("A new signing secret was created.")
        } else {
            None
        }
    });
    let secret = if subscriber.kind == SubscriberKind::Webhook
        && subscriber.status == SubscriberStatus::Active
    {
        state.store.reveal_webhook_secret(subscriber.id).await?
    } else {
        None
    };
    let (_, choices) = subscribable(cx)
        .await
        .unwrap_or_else(|_| (view.clone(), Vec::new()));
    let base = base(cx, &view.slug);
    let token = token.unwrap_or_default();
    let active = subscriber.status == SubscriberStatus::Active;
    let who = match (&subscriber.kind, &subscriber.webhook_url) {
        (SubscriberKind::Webhook, Some(url)) => format!(
            "Webhook to {} (owner {})",
            url.host_str().unwrap_or_default(),
            subscriber.email.masked()
        ),
        _ => format!("Email to {}", subscriber.email.masked()),
    };
    respond(
        cx,
        &view,
        StatusCode::OK,
        "Your subscription",
        view! { cx =>
            <section class="subscribe-done">
                <h2>"Your subscription"</h2>
                if let Some(notice) = notice {
                    <p class="flash" role="status">(notice)</p>
                }
                <p>(who.as_str()) " · " (subscriber.status.label())</p>
                if let Some(secret) = &secret {
                    <div class="flash">
                        <p><strong>"Signing secret"</strong> " (shown once; store it now):"</p>
                        <p><code>(secret.as_str())</code></p>
                        <p class="small">"Each request has X-Uptimestatus-Timestamp and X-Uptimestatus-Signature: sha256=HMAC-SHA256(secret, timestamp + \".\" + body), hex."</p>
                    </div>
                }
                if active {
                    <form method="post" action=(format!("{base}/subscriptions/manage"))>
                        <input type="hidden" name="token" value=(token.as_str())>
                        component_choices(choices: &choices, selection: &subscriber.selection)
                        <div class="actions"><button class="btn btn-primary" type="submit">"Save"</button></div>
                    </form>
                    if subscriber.kind == SubscriberKind::Webhook {
                        <form method="post" action=(format!("{base}/subscriptions/secret"))>
                            <input type="hidden" name="token" value=(token.as_str())>
                            <button class="btn" type="submit">"Create a new signing secret"</button>
                        </form>
                    }
                    <form method="post" action=(format!("{base}/subscriptions/unsubscribe"))>
                        <input type="hidden" name="token" value=(token.as_str())>
                        <button class="btn btn-danger" type="submit">"Unsubscribe"</button>
                    </form>
                }
            </section>
        }
        .single()
        .await?
        .render(cx),
    )
    .await
}

#[route(POST "/s/{slug}/subscriptions/manage")]
pub(crate) async fn save_subscription(cx: &Cx, Form(input): Form<TokenForm>) -> Result<Response> {
    let view = load(cx, path_param::<Slug>(cx)).await?;
    let Some(subscriber) = open(cx, &view, Some(&input.token), LinkPurpose::Manage).await? else {
        return invalid_link(cx, &view).await;
    };
    let (_, choices) = subscribable(cx).await?;
    let form = SubscribeForm {
        scope: input.scope.clone(),
        extra: input.extra.clone(),
        ..SubscribeForm::default()
    };
    app(cx)
        .store
        .set_subscriber_selection(subscriber.id, &form.selection(&choices))
        .await?;
    let base = base(cx, &view.slug);
    see_other(manage_href(&base, &input.token, "saved")).into_response(cx)
}

#[route(POST "/s/{slug}/subscriptions/secret")]
pub(crate) async fn rotate_secret(cx: &Cx, Form(input): Form<TokenForm>) -> Result<SeeOther> {
    let view = load(cx, path_param::<Slug>(cx)).await?;
    let Some(subscriber) = open(cx, &view, Some(&input.token), LinkPurpose::Manage).await? else {
        return Err(not_found().into());
    };
    app(cx).store.rotate_webhook_secret(subscriber.id).await?;
    let base = base(cx, &view.slug);
    Ok(see_other(manage_href(&base, &input.token, "rotated")))
}

/// The unsubscribe link: a button (mail clients use the one-click POST).
#[route(GET "/s/{slug}/subscriptions/unsubscribe")]
pub(crate) async fn unsubscribe_page(cx: &Cx) -> Result<Response> {
    let view = load(cx, path_param::<Slug>(cx)).await?;
    let token = query_token(cx);
    if open(cx, &view, token.as_deref(), LinkPurpose::Unsubscribe)
        .await?
        .is_none()
    {
        return invalid_link(cx, &view).await;
    }
    let base = base(cx, &view.slug);
    let token = token.unwrap_or_default();
    let question = format!("Stop receiving updates from {}?", view.title);
    respond(
        cx,
        &view,
        StatusCode::OK,
        "Unsubscribe",
        view! { cx =>
            <section class="subscribe-done">
                <h2>"Unsubscribe"</h2>
                <p>(question.as_str())</p>
                <form method="post" action=(format!("{base}/subscriptions/unsubscribe"))>
                    <input type="hidden" name="token" value=(token.as_str())>
                    <button class="btn btn-danger" type="submit">"Unsubscribe"</button>
                </form>
            </section>
        }
        .single()
        .await?
        .render(cx),
    )
    .await
}

/// Unsubscribes with an unsubscribe token (one-click, `?token=`) or a manage
/// token (the form on the manage page).
#[route(POST "/s/{slug}/subscriptions/unsubscribe")]
pub(crate) async fn unsubscribe(cx: &Cx, Form(input): Form<TokenForm>) -> Result<Response> {
    let view = load(cx, path_param::<Slug>(cx)).await?;
    let token = query_token(cx).or_else(|| Some(input.token.clone()).filter(|t| !t.is_empty()));
    let mut subscriber = open(cx, &view, token.as_deref(), LinkPurpose::Unsubscribe).await?;
    if subscriber.is_none() {
        subscriber = open(cx, &view, token.as_deref(), LinkPurpose::Manage).await?;
    }
    let Some(subscriber) = subscriber else {
        return invalid_link(cx, &view).await;
    };
    app(cx)
        .store
        .unsubscribe(subscriber.id, Timestamp::now())
        .await?;
    let base = base(cx, &view.slug);
    let done = format!("You will not receive updates from {} any more.", view.title);
    respond(
        cx,
        &view,
        StatusCode::OK,
        "Unsubscribed",
        view! { cx =>
            <section class="subscribe-done">
                <h2>"You are unsubscribed"</h2>
                <p>(done.as_str())</p>
                <p><a href=(format!("{base}/subscribe"))>"Subscribe again"</a></p>
            </section>
        }
        .single()
        .await?
        .render(cx),
    )
    .await
}
