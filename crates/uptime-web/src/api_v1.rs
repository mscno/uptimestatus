//! The JSON API, `/api/v1`: monitors, status pages and incidents, for
//! automation (Terraform-style `PUT` upserts, CI hooks, scripts).
//!
//! Requests carry `Authorization: Bearer upt_…`; tokens are created in the
//! console and come in two scopes: *read* (which sees secrets redacted) and
//! *write*. Errors are `{"error": "…"}`.

use std::time::Duration;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use topcoat::{
    Result,
    context::Cx,
    router::{
        HeaderValue, StatusCode,
        content::Json,
        header, path_param, query_params,
        request::headers,
        response::{IntoResponse as _, Response},
        route,
    },
};
use uptime_domain::{
    Impact, IncidentStatus, MonitorKey, MonitorSpec, MonitorState, PageSlug, PageSpec, TokenScope,
    bearer_token, normalize_group, normalize_tags,
};
use uptime_store::{ApiToken, Monitor, MonitorOverview, NewIncident, StoreError};

use crate::{
    audit,
    auth::app,
    publish::{self, Via},
};

/// SHA-256 of a token, as stored.
pub fn hash_token(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// A fresh token: `upt_` and 32 random bytes, base64url.
pub fn new_token() -> String {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    let mut bytes = [0u8; 32];
    // Without OS randomness nothing here can be secure, so fail loudly.
    if let Err(error) = getrandom::fill(&mut bytes) {
        panic!("the OS random number generator is unavailable: {error}");
    }
    format!(
        "{}{}",
        uptime_domain::TOKEN_PREFIX,
        URL_SAFE_NO_PAD.encode(bytes)
    )
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

fn fail(cx: &Cx, status: StatusCode, message: impl Into<String>) -> Result<Response> {
    (
        status,
        Json(ErrorBody {
            error: message.into(),
        }),
    )
        .into_response(cx)
}

/// The token behind the request, or the error response to send.
async fn authorize(cx: &Cx, needed: TokenScope) -> Result<std::result::Result<ApiToken, Response>> {
    let token = headers(cx)
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(bearer_token);
    let Some(token) = token else {
        let mut response = fail(
            cx,
            StatusCode::UNAUTHORIZED,
            "send `Authorization: Bearer <token>`",
        )?;
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        return Ok(Err(response));
    };
    let found = app(cx)
        .store
        .authenticate_api_token(&hash_token(token), Timestamp::now())
        .await?;
    match found {
        None => {
            let mut response = fail(cx, StatusCode::UNAUTHORIZED, "unknown or revoked token")?;
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
            Ok(Err(response))
        }
        Some(token) if !token.scope.permits(needed) => Ok(Err(fail(
            cx,
            StatusCode::FORBIDDEN,
            "this token is read-only",
        )?)),
        Some(token) => Ok(Ok(token)),
    }
}

macro_rules! authorized {
    ($cx:expr, $scope:expr) => {
        match authorize($cx, $scope).await? {
            Ok(token) => token,
            Err(response) => return Ok(response),
        }
    };
}

#[derive(Serialize)]
struct MonitorBody {
    #[serde(flatten)]
    spec: MonitorSpec,
    state: MonitorState,
    last_checked_at: Option<Timestamp>,
    last_latency_ms: Option<i64>,
    last_error: Option<String>,
    state_changed_at: Option<Timestamp>,
}

fn monitor_body(overview: MonitorOverview, scope: TokenScope) -> MonitorBody {
    let MonitorOverview { monitor, runtime } = overview;
    let mut spec = monitor.spec;
    if scope == TokenScope::Read {
        spec = spec.redacted();
    }
    MonitorBody {
        spec,
        state: runtime.runtime.state,
        last_checked_at: runtime.last_checked_at,
        last_latency_ms: runtime.last_latency_ms,
        // Transport errors can echo the request URL or a remote response.
        last_error: (scope == TokenScope::Write)
            .then_some(runtime.last_error)
            .flatten(),
        state_changed_at: runtime.state_changed_at,
    }
}

path_param!(key);
path_param!(slug);
path_param!(id: i64, error = not_found);

#[query_params]
pub(crate) struct ListQuery {
    tag: Option<String>,
    /// A group path: matches that group and everything below it.
    group: Option<String>,
    q: Option<String>,
    limit: Option<String>,
}

async fn find(cx: &Cx, key: &str) -> Result<Option<MonitorOverview>> {
    Ok(app(cx)
        .store
        .monitor_overviews()
        .await?
        .into_iter()
        .find(|o| o.monitor.spec.key.as_str() == key))
}

#[route(GET "/api/v1/monitors")]
pub(crate) async fn list_monitors(cx: &Cx) -> Result<Response> {
    let token = authorized!(cx, TokenScope::Read);
    let query = query_params::<ListQuery>(cx).ok();
    let tag = query.and_then(|q| q.tag.as_deref()).map(str::to_lowercase);
    let text = query.and_then(|q| q.q.as_deref()).map(str::to_lowercase);
    let group = query
        .and_then(|q| q.group.as_deref())
        .and_then(|g| normalize_group(g).ok().flatten());
    let monitors: Vec<MonitorBody> = app(cx)
        .store
        .monitor_overviews()
        .await?
        .into_iter()
        .filter(|o| {
            let spec = &o.monitor.spec;
            tag.as_ref().is_none_or(|tag| spec.tags.contains(tag))
                && group.as_ref().is_none_or(|group| {
                    spec.group
                        .as_ref()
                        .is_some_and(|own| own == group || own.starts_with(&format!("{group}/")))
                })
                && text.as_ref().is_none_or(|text| {
                    spec.name.to_lowercase().contains(text.as_str())
                        || spec.key.as_str().contains(text.as_str())
                })
        })
        .map(|o| monitor_body(o, token.scope))
        .collect();
    Json(serde_json::json!({ "monitors": monitors })).into_response(cx)
}

#[route(GET "/api/v1/monitors/{key}")]
pub(crate) async fn get_monitor(cx: &Cx) -> Result<Response> {
    let token = authorized!(cx, TokenScope::Read);
    match find(cx, path_param::<Key>(cx)).await? {
        Some(overview) => Json(monitor_body(overview, token.scope)).into_response(cx),
        None => fail(cx, StatusCode::NOT_FOUND, "no such monitor"),
    }
}

/// Parses and checks a monitor body; the key comes from the URL.
fn parse_monitor(key: &str, mut body: Value) -> std::result::Result<MonitorSpec, String> {
    let object = body
        .as_object_mut()
        .ok_or("the body must be a JSON object")?;
    match object.get("key") {
        None => {
            object.insert("key".into(), Value::String(key.to_owned()));
        }
        Some(Value::String(given)) if given == key => {}
        Some(_) => return Err("`key` in the body must match the URL".into()),
    }
    let mut spec: MonitorSpec = serde_json::from_value(body).map_err(|e| e.to_string())?;
    spec.key
        .as_str()
        .parse::<MonitorKey>()
        .map_err(|e| e.to_string())?;
    spec.policy.validate().map_err(|e| e.to_string())?;
    spec.check.validate().map_err(|e| e.to_string())?;
    spec.tags = normalize_tags(&spec.tags.join(" ")).map_err(|e| e.to_string())?;
    spec.group = match spec.group.as_deref() {
        Some(group) => normalize_group(group).map_err(|e| e.to_string())?,
        None => None,
    };
    if let uptime_domain::CheckSpec::Http(http) = &spec.check
        && let Some(rule) = &http.json
    {
        rule.validate().map_err(|e| e.to_string())?;
    }
    Ok(spec)
}

/// Creates or replaces the monitor with this key.
#[route(PUT "/api/v1/monitors/{key}")]
pub(crate) async fn put_monitor(cx: &Cx, Json(body): Json<Value>) -> Result<Response> {
    let token = authorized!(cx, TokenScope::Write);
    let key = path_param::<Key>(cx);
    let spec = match parse_monitor(key, body) {
        Ok(spec) => spec,
        Err(message) => return fail(cx, StatusCode::UNPROCESSABLE_ENTITY, message),
    };
    let state = app(cx);
    let now = Timestamp::now();
    let (status, id) = match find(cx, key).await? {
        Some(existing) => {
            let id = existing.monitor.id;
            match state.store.update_monitor(id, &spec, now).await {
                Ok(_) => {}
                Err(error @ StoreError::GroupCycle(_)) => {
                    return fail(cx, StatusCode::UNPROCESSABLE_ENTITY, error.to_string());
                }
                Err(error) => return Err(error.into()),
            }
            let before = existing.monitor.spec;
            let details = audit::changes(&before, &spec, &before.redacted(), &spec.redacted());
            audit::record(
                state,
                audit::api(&token),
                "monitor.update",
                audit::target("monitor", id),
                format!(
                    "Updated monitor {} ({})",
                    spec.name,
                    audit::changed_fields(&details).join(", ")
                ),
                details,
            )
            .await?;
            (StatusCode::OK, id)
        }
        None => {
            let first_run = uptime_runtime::schedule::first_run_at(now, &spec.key, Duration::ZERO);
            match state.store.create_monitor(&spec, first_run).await {
                Ok(Monitor { id, .. }) => {
                    audit::record(
                        state,
                        audit::api(&token),
                        "monitor.create",
                        audit::target("monitor", id),
                        format!("Created monitor {} ({})", spec.name, spec.key),
                        serde_json::json!({ "monitor": audit::value(&spec.redacted()) }),
                    )
                    .await?;
                    (StatusCode::CREATED, id)
                }
                Err(StoreError::DuplicateKey(_)) => {
                    return fail(cx, StatusCode::CONFLICT, "a monitor with this key exists");
                }
                Err(error @ StoreError::GroupCycle(_)) => {
                    return fail(cx, StatusCode::UNPROCESSABLE_ENTITY, error.to_string());
                }
                Err(error) => return Err(error.into()),
            }
        }
    };
    state.wake_scheduler();
    state.pages.clear();
    tracing::info!(monitor = %id, key, "monitor upserted through the API");
    let overview = find(cx, key)
        .await?
        .map(|o| monitor_body(o, TokenScope::Write));
    (status, Json(overview)).into_response(cx)
}

#[route(DELETE "/api/v1/monitors/{key}")]
pub(crate) async fn delete_monitor(cx: &Cx) -> Result<Response> {
    let token = authorized!(cx, TokenScope::Write);
    let Some(overview) = find(cx, path_param::<Key>(cx)).await? else {
        return fail(cx, StatusCode::NOT_FOUND, "no such monitor");
    };
    app(cx).store.delete_monitor(overview.monitor.id).await?;
    app(cx).pages.clear();
    let spec = &overview.monitor.spec;
    audit::record(
        app(cx),
        audit::api(&token),
        "monitor.delete",
        audit::target("monitor", overview.monitor.id),
        format!("Deleted monitor {} ({})", spec.name, spec.key),
        serde_json::json!({ "monitor": audit::value(&spec.redacted()) }),
    )
    .await?;
    StatusCode::NO_CONTENT.into_response(cx)
}

async fn set_active(cx: &Cx, active: bool) -> Result<Response> {
    let token = authorized!(cx, TokenScope::Write);
    let Some(overview) = find(cx, path_param::<Key>(cx)).await? else {
        return fail(cx, StatusCode::NOT_FOUND, "no such monitor");
    };
    let state = app(cx);
    state
        .store
        .set_monitor_active(overview.monitor.id, active, Timestamp::now())
        .await?;
    if active {
        state.wake_scheduler();
    }
    state.pages.clear();
    audit::record(
        state,
        audit::api(&token),
        if active {
            "monitor.resume"
        } else {
            "monitor.pause"
        },
        audit::target("monitor", overview.monitor.id),
        format!(
            "{} monitor {}",
            if active { "Resumed" } else { "Paused" },
            overview.monitor.spec.name
        ),
        Value::Null,
    )
    .await?;
    StatusCode::NO_CONTENT.into_response(cx)
}

#[route(POST "/api/v1/monitors/{key}/pause")]
pub(crate) async fn pause_monitor(cx: &Cx) -> Result<Response> {
    set_active(cx, false).await
}

#[route(POST "/api/v1/monitors/{key}/resume")]
pub(crate) async fn resume_monitor(cx: &Cx) -> Result<Response> {
    set_active(cx, true).await
}

#[derive(Serialize)]
struct CheckBody {
    checked_at: Timestamp,
    health: &'static str,
    state: &'static str,
    latency_ms: Option<i64>,
    status_code: Option<u16>,
    error: Option<String>,
    region: String,
}

/// Recent check results, newest first (`?limit=`, at most 500).
#[route(GET "/api/v1/monitors/{key}/checks")]
pub(crate) async fn monitor_checks(cx: &Cx) -> Result<Response> {
    let token = authorized!(cx, TokenScope::Read);
    let Some(overview) = find(cx, path_param::<Key>(cx)).await? else {
        return fail(cx, StatusCode::NOT_FOUND, "no such monitor");
    };
    let limit = query_params::<ListQuery>(cx)
        .ok()
        .and_then(|q| q.limit.as_deref()?.parse::<u32>().ok())
        .unwrap_or(50)
        .clamp(1, 500);
    let checks: Vec<CheckBody> = app(cx)
        .store
        .recent_checks(overview.monitor.id, limit)
        .await?
        .into_iter()
        .map(|c| CheckBody {
            checked_at: c.checked_at,
            health: c.health.as_str(),
            state: c.state_after.as_str(),
            latency_ms: c.latency_ms,
            status_code: c.status_code,
            error: (token.scope == TokenScope::Write)
                .then_some(c.error)
                .flatten(),
            region: c.region,
        })
        .collect();
    Json(serde_json::json!({ "checks": checks })).into_response(cx)
}

/// Every monitor and page, in the shape `PUT` accepts (secrets redacted for
/// read-only tokens).
#[route(GET "/api/v1/config")]
pub(crate) async fn get_config(cx: &Cx) -> Result<Response> {
    let token = authorized!(cx, TokenScope::Read);
    let mut config = app(cx).store.export_config().await?;
    if token.scope == TokenScope::Read {
        config.monitors = config.monitors.iter().map(MonitorSpec::redacted).collect();
    }
    Json(serde_json::json!({ "monitors": config.monitors, "pages": config.pages }))
        .into_response(cx)
}

/// A page component and the status admins set on it.
#[derive(Serialize)]
struct ComponentStatusBody {
    key: String,
    /// `monitor` or `manual`.
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    /// A manual component's status, or a monitor component's override;
    /// `null` when a monitor component follows its checks.
    status: Option<uptime_domain::ComponentStatus>,
    /// When the status was last set, by whom, and why.
    #[serde(skip_serializing_if = "Option::is_none")]
    since: Option<Timestamp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    actor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

/// The page's components with their manual statuses and overrides.
#[route(GET "/api/v1/pages/{slug}/components")]
pub(crate) async fn page_components(cx: &Cx) -> Result<Response> {
    authorized!(cx, TokenScope::Read);
    let store = &app(cx).store;
    let Some(page) = store.page_by_slug(path_param::<Slug>(cx)).await? else {
        return fail(cx, StatusCode::NOT_FOUND, "no such page");
    };
    let events = store.component_events(page.id, usize::MAX).await?;
    let components: Vec<ComponentStatusBody> = page
        .spec
        .components()
        .map(|component| {
            let latest = events.iter().find(|e| e.key == component.key().as_str());
            ComponentStatusBody {
                key: component.key().to_string(),
                kind: if component.is_manual() {
                    "manual"
                } else {
                    "monitor"
                },
                label: component.label().map(str::to_owned),
                status: latest.and_then(|e| e.status),
                since: latest.map(|e| e.at),
                actor: latest.map(|e| e.actor.clone()),
                note: latest.and_then(|e| e.note.clone()),
            }
        })
        .collect();
    Json(serde_json::json!({ "components": components })).into_response(cx)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusBody {
    /// `null` releases a monitor component's override.
    status: Option<uptime_domain::ComponentStatus>,
    #[serde(default)]
    note: Option<String>,
}

/// Sets a manual component's status, or overrides (or with `null`, releases)
/// a monitor component's.
#[route(PUT "/api/v1/pages/{slug}/components/{key}/status")]
pub(crate) async fn set_component_status(cx: &Cx, Json(body): Json<Value>) -> Result<Response> {
    let token = authorized!(cx, TokenScope::Write);
    let body: StatusBody = match serde_json::from_value(body) {
        Ok(body) => body,
        Err(error) => return fail(cx, StatusCode::UNPROCESSABLE_ENTITY, error.to_string()),
    };
    let state = app(cx);
    let Some(page) = state.store.page_by_slug(path_param::<Slug>(cx)).await? else {
        return fail(cx, StatusCode::NOT_FOUND, "no such page");
    };
    let key = path_param::<Key>(cx);
    let event = match state
        .store
        .set_component_status(
            page.id,
            key,
            body.status,
            body.note.as_deref(),
            &audit::api(&token),
            Timestamp::now(),
        )
        .await
    {
        Ok(event) => event,
        Err(StoreError::ComponentNotFound(_)) => {
            return fail(cx, StatusCode::NOT_FOUND, "no such component on this page");
        }
        Err(StoreError::StatusRequired(_)) => {
            return fail(
                cx,
                StatusCode::UNPROCESSABLE_ENTITY,
                "manual components always have a status",
            );
        }
        Err(error) => return Err(error.into()),
    };
    state.page_changed(page.spec.slug.as_str());
    crate::admin::record_component_status(state, audit::api(&token), &page, &event).await?;
    Json(ComponentStatusBody {
        key: event.key,
        kind: if page.spec.component(key).is_some_and(|c| c.is_manual()) {
            "manual"
        } else {
            "monitor"
        },
        label: page
            .spec
            .component(key)
            .and_then(|c| c.label())
            .map(str::to_owned),
        status: event.status,
        since: Some(event.at),
        actor: Some(event.actor),
        note: event.note,
    })
    .into_response(cx)
}

#[route(GET "/api/v1/pages/{slug}")]
pub(crate) async fn get_page(cx: &Cx) -> Result<Response> {
    authorized!(cx, TokenScope::Read);
    match app(cx).store.page_by_slug(path_param::<Slug>(cx)).await? {
        Some(page) => Json(page.spec).into_response(cx),
        None => fail(cx, StatusCode::NOT_FOUND, "no such page"),
    }
}

fn store_failure(cx: &Cx, error: StoreError) -> Result<Response> {
    match error {
        StoreError::UnknownMonitors(keys) => fail(
            cx,
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "no monitors with the keys {}",
                keys.iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ),
        StoreError::DuplicateSlug(_) => {
            fail(cx, StatusCode::CONFLICT, "a page with this slug exists")
        }
        StoreError::InvalidPage(message) => fail(cx, StatusCode::UNPROCESSABLE_ENTITY, message),
        other => Err(other.into()),
    }
}

/// Creates or replaces the status page with this slug.
#[route(PUT "/api/v1/pages/{slug}")]
pub(crate) async fn put_page(cx: &Cx, Json(mut body): Json<Value>) -> Result<Response> {
    let token = authorized!(cx, TokenScope::Write);
    let slug = path_param::<Slug>(cx);
    let Some(object) = body.as_object_mut() else {
        return fail(
            cx,
            StatusCode::UNPROCESSABLE_ENTITY,
            "the body must be a JSON object",
        );
    };
    match object.get("slug") {
        None => {
            object.insert("slug".into(), Value::String(slug.to_owned()));
        }
        Some(Value::String(given)) if given == slug => {}
        Some(_) => {
            return fail(
                cx,
                StatusCode::UNPROCESSABLE_ENTITY,
                "`slug` in the body must match the URL",
            );
        }
    }
    let spec: PageSpec = match serde_json::from_value(body) {
        Ok(spec) => spec,
        Err(error) => return fail(cx, StatusCode::UNPROCESSABLE_ENTITY, error.to_string()),
    };
    if spec.slug.as_str().parse::<PageSlug>().is_err() {
        return fail(cx, StatusCode::UNPROCESSABLE_ENTITY, "invalid slug");
    }
    let state = app(cx);
    let existing = state.store.page_by_slug(slug).await?;
    let before = existing.as_ref().map(|page| page.spec.clone());
    let result = match existing {
        Some(existing) => state
            .store
            .update_page(existing.id, &spec)
            .await
            .map(|page| (StatusCode::OK, page)),
        None => state
            .store
            .create_page(&spec)
            .await
            .map(|page| (StatusCode::CREATED, page)),
    };
    match result {
        Ok((status, page)) => {
            state.pages.clear();
            let (action, summary, details) = match &before {
                Some(before) => {
                    let details = audit::page_changes(before, &page.spec);
                    let summary = format!(
                        "Updated status page {} ({})",
                        page.spec.title,
                        audit::changed_fields(&details).join(", ")
                    );
                    ("page.update", summary, details)
                }
                None => (
                    "page.create",
                    format!(
                        "Created status page {} (/s/{})",
                        page.spec.title, page.spec.slug
                    ),
                    serde_json::json!({ "page": audit::page(&page.spec) }),
                ),
            };
            audit::record(
                state,
                audit::api(&token),
                action,
                audit::target("page", page.id),
                summary,
                details,
            )
            .await?;
            (status, Json(page.spec)).into_response(cx)
        }
        Err(error) => store_failure(cx, error),
    }
}

#[route(DELETE "/api/v1/pages/{slug}")]
pub(crate) async fn delete_page(cx: &Cx) -> Result<Response> {
    let token = authorized!(cx, TokenScope::Write);
    let state = app(cx);
    let Some(page) = state.store.page_by_slug(path_param::<Slug>(cx)).await? else {
        return fail(cx, StatusCode::NOT_FOUND, "no such page");
    };
    state.store.delete_page(page.id).await?;
    state.pages.clear();
    audit::record(
        state,
        audit::api(&token),
        "page.delete",
        audit::target("page", page.id),
        format!(
            "Deleted status page {} (/s/{})",
            page.spec.title, page.spec.slug
        ),
        serde_json::json!({ "page": audit::page(&page.spec) }),
    )
    .await?;
    StatusCode::NO_CONTENT.into_response(cx)
}

#[derive(Deserialize)]
struct IncidentInput {
    /// An incident template's name: fills what is not given.
    #[serde(default)]
    template: Option<String>,
    #[serde(default)]
    title: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    impact: Option<Impact>,
    #[serde(default)]
    status: Option<IncidentStatus>,
    monitors: Vec<MonitorKey>,
}

/// The template named `name`, or a 422 reply.
async fn named_template(
    cx: &Cx,
    name: Option<&str>,
) -> Result<std::result::Result<Option<uptime_domain::IncidentTemplate>, Response>> {
    let Some(name) = name else {
        return Ok(Ok(None));
    };
    match app(cx).store.template_by_name(name).await? {
        Some(template) => Ok(Ok(Some(template.spec))),
        None => Ok(Err(fail(
            cx,
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("no incident template named `{name}`"),
        )?)),
    }
}

#[derive(Serialize)]
struct IncidentBody {
    id: i64,
    title: String,
    impact: Impact,
    status: IncidentStatus,
    started_at: Timestamp,
    resolved_at: Option<Timestamp>,
}

impl From<uptime_store::Incident> for IncidentBody {
    fn from(incident: uptime_store::Incident) -> Self {
        Self {
            id: incident.id,
            title: incident.title,
            impact: incident.impact,
            status: incident.status,
            started_at: incident.started_at,
            resolved_at: incident.resolved_at,
        }
    }
}

/// Declares an incident (for alerting integrations and runbooks).
#[route(POST "/api/v1/incidents")]
pub(crate) async fn create_incident(cx: &Cx, Json(input): Json<IncidentInput>) -> Result<Response> {
    let token = authorized!(cx, TokenScope::Write);
    let template = match named_template(cx, input.template.as_deref()).await? {
        Ok(template) => template,
        Err(reply) => return Ok(reply),
    };
    let mut input = input;
    if let Some(template) = template {
        if input.title.trim().is_empty() {
            input.title = template.title;
        }
        if input.message.trim().is_empty() {
            input.message = template.body;
        }
        input.impact = input.impact.or(Some(template.impact));
        input.status = input.status.or(Some(template.status));
    }
    if input.title.trim().is_empty() || input.message.trim().is_empty() {
        return fail(
            cx,
            StatusCode::UNPROCESSABLE_ENTITY,
            "`title` and `message` are required",
        );
    }
    if input.monitors.is_empty() {
        return fail(
            cx,
            StatusCode::UNPROCESSABLE_ENTITY,
            "name at least one monitor",
        );
    }
    let state = app(cx);
    let new = NewIncident {
        title: input.title.trim().to_owned(),
        impact: input.impact.unwrap_or_default(),
        status: input.status.unwrap_or_default(),
        message: input.message.trim().to_owned(),
        monitors: input.monitors,
    };
    match publish::declare(
        state,
        &audit::api(&token),
        &new,
        Via::Direct,
        Timestamp::now(),
    )
    .await?
    {
        Ok(incident) => (StatusCode::CREATED, Json(IncidentBody::from(incident))).into_response(cx),
        Err(error) => store_failure(cx, error),
    }
}

#[derive(Deserialize)]
struct UpdateInput {
    /// An incident template's name: fills what is not given.
    #[serde(default)]
    template: Option<String>,
    #[serde(default)]
    status: Option<IncidentStatus>,
    #[serde(default)]
    message: Option<String>,
}

/// Posts an update (and moves the incident's status).
#[route(POST "/api/v1/incidents/{id}/updates")]
pub(crate) async fn post_incident_update(
    cx: &Cx,
    Json(input): Json<UpdateInput>,
) -> Result<Response> {
    let token = authorized!(cx, TokenScope::Write);
    let id = *path_param::<Id>(cx)?;
    let template = match named_template(cx, input.template.as_deref()).await? {
        Ok(template) => template,
        Err(reply) => return Ok(reply),
    };
    let Some(status) = input.status.or_else(|| template.as_ref().map(|t| t.status)) else {
        return fail(
            cx,
            StatusCode::UNPROCESSABLE_ENTITY,
            "`status` is required (or name a `template`)",
        );
    };
    let message = input
        .message
        .filter(|m| !m.trim().is_empty())
        .or_else(|| template.map(|t| t.body))
        .unwrap_or_else(|| format!("Status changed to {}.", status.label().to_lowercase()));
    let state = app(cx);
    match publish::update(
        state,
        &audit::api(&token),
        id,
        status,
        &message,
        Via::Direct,
        Timestamp::now(),
    )
    .await?
    {
        Ok(incident) => Json(IncidentBody::from(incident)).into_response(cx),
        Err(StoreError::IncidentNotFound(_)) => fail(cx, StatusCode::NOT_FOUND, "no such incident"),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn tokens_are_prefixed_unique_and_hash_stably() {
        let (a, b) = (new_token(), new_token());
        assert!(a.starts_with("upt_") && a.len() > 40);
        assert_ne!(a, b);
        assert_eq!(hash_token(&a), hash_token(&a));
        assert_ne!(hash_token(&a), hash_token(&b));
    }

    #[test]
    fn bodies_take_their_key_from_the_url() {
        let body = serde_json::json!({
            "name": "API",
            "check": {"type": "tcp", "host": "db.example.com", "port": 5432},
            "tags": ["Prod", "api"]
        });
        let spec = parse_monitor("api", body.clone()).unwrap();
        assert_eq!(spec.key.as_str(), "api");
        assert_eq!(spec.tags, ["api", "prod"]);

        let mut mismatched = body;
        mismatched["key"] = "other".into();
        assert!(parse_monitor("api", mismatched).is_err());
        assert!(parse_monitor("api", serde_json::json!([1])).is_err());
    }
}
