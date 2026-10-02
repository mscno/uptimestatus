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

use crate::auth::app;

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

/// Request and match values are arbitrary user input and can contain secrets.
/// A read token gets the check shape and destination host, but not those values.
fn redact(spec: &mut MonitorSpec) {
    use uptime_domain::{CheckSpec, HttpAuth};
    const MASK: &str = "***";
    match &mut spec.check {
        CheckSpec::Http(http) => {
            // Paths, query strings and URL userinfo can all carry credentials.
            let _ = http.url.set_username("");
            let _ = http.url.set_password(None);
            http.url.set_path("/[redacted]");
            http.url.set_query(None);
            http.url.set_fragment(None);
            for (_, value) in &mut http.headers {
                *value = MASK.into();
            }
            if let Some(body) = &mut http.body {
                *body = MASK.into();
            }
            match &mut http.auth {
                Some(HttpAuth::Basic { username, password }) => {
                    *username = MASK.into();
                    *password = MASK.into();
                }
                Some(HttpAuth::Bearer { token }) => *token = MASK.into(),
                None => {}
            }
            if let Some(keyword) = &mut http.keyword {
                keyword.text = MASK.into();
            }
            if let Some(rule) = &mut http.json {
                rule.path = MASK.into();
                if let Some(expect) = &mut rule.expect {
                    *expect = MASK.into();
                }
            }
        }
        CheckSpec::Tcp(tcp) => {
            tcp.send = tcp.send.as_ref().map(|_| MASK.into());
            tcp.expect = tcp.expect.as_ref().map(|_| MASK.into());
        }
        CheckSpec::Dns(dns) => dns.expect = dns.expect.as_ref().map(|_| MASK.into()),
        CheckSpec::Push(push) => push.token = MASK.into(),
    }
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
        redact(&mut spec);
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
    authorized!(cx, TokenScope::Write);
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
            state.store.update_monitor(id, &spec, now).await?;
            (StatusCode::OK, id)
        }
        None => {
            let first_run = uptime_runtime::schedule::first_run_at(now, &spec.key, Duration::ZERO);
            match state.store.create_monitor(&spec, first_run).await {
                Ok(Monitor { id, .. }) => (StatusCode::CREATED, id),
                Err(StoreError::DuplicateKey(_)) => {
                    return fail(cx, StatusCode::CONFLICT, "a monitor with this key exists");
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
    authorized!(cx, TokenScope::Write);
    let Some(overview) = find(cx, path_param::<Key>(cx)).await? else {
        return fail(cx, StatusCode::NOT_FOUND, "no such monitor");
    };
    app(cx).store.delete_monitor(overview.monitor.id).await?;
    app(cx).pages.clear();
    StatusCode::NO_CONTENT.into_response(cx)
}

async fn set_active(cx: &Cx, active: bool) -> Result<Response> {
    authorized!(cx, TokenScope::Write);
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
        config.monitors.iter_mut().for_each(redact);
    }
    Json(serde_json::json!({ "monitors": config.monitors, "pages": config.pages }))
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
        other => Err(other.into()),
    }
}

/// Creates or replaces the status page with this slug.
#[route(PUT "/api/v1/pages/{slug}")]
pub(crate) async fn put_page(cx: &Cx, Json(mut body): Json<Value>) -> Result<Response> {
    authorized!(cx, TokenScope::Write);
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
    let result = match state.store.page_by_slug(slug).await? {
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
            (status, Json(page.spec)).into_response(cx)
        }
        Err(error) => store_failure(cx, error),
    }
}

#[route(DELETE "/api/v1/pages/{slug}")]
pub(crate) async fn delete_page(cx: &Cx) -> Result<Response> {
    authorized!(cx, TokenScope::Write);
    let state = app(cx);
    let Some(page) = state.store.page_by_slug(path_param::<Slug>(cx)).await? else {
        return fail(cx, StatusCode::NOT_FOUND, "no such page");
    };
    state.store.delete_page(page.id).await?;
    state.pages.clear();
    StatusCode::NO_CONTENT.into_response(cx)
}

#[derive(Deserialize)]
struct IncidentInput {
    title: String,
    message: String,
    #[serde(default)]
    impact: Option<Impact>,
    #[serde(default)]
    status: Option<IncidentStatus>,
    monitors: Vec<MonitorKey>,
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
    authorized!(cx, TokenScope::Write);
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
    match state.store.declare_incident(&new, Timestamp::now()).await {
        Ok(incident) => {
            state.pages.clear();
            (StatusCode::CREATED, Json(IncidentBody::from(incident))).into_response(cx)
        }
        Err(error) => store_failure(cx, error),
    }
}

#[derive(Deserialize)]
struct UpdateInput {
    status: IncidentStatus,
    #[serde(default)]
    message: Option<String>,
}

/// Posts an update (and moves the incident's status).
#[route(POST "/api/v1/incidents/{id}/updates")]
pub(crate) async fn post_incident_update(
    cx: &Cx,
    Json(input): Json<UpdateInput>,
) -> Result<Response> {
    authorized!(cx, TokenScope::Write);
    let id = *path_param::<Id>(cx)?;
    let message = input
        .message
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| format!("Status changed to {}.", input.status.label().to_lowercase()));
    let state = app(cx);
    match state
        .store
        .post_incident_update(id, input.status, &message, Timestamp::now())
        .await
    {
        Ok(incident) => {
            state.pages.clear();
            Json(IncidentBody::from(incident)).into_response(cx)
        }
        Err(StoreError::IncidentNotFound(_)) => fail(cx, StatusCode::NOT_FOUND, "no such incident"),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use uptime_domain::{CheckPolicy, CheckSpec, HttpAuth, HttpCheck};

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
    fn redaction_hides_arbitrary_request_values() {
        let mut http = HttpCheck::get("https://example.com".parse().unwrap());
        http.auth = Some(HttpAuth::Basic {
            username: "bob".into(),
            password: "hunter2".into(),
        });
        http.headers = vec![
            ("X-Api-Key".into(), "sekret".into()),
            ("Accept".into(), "text/html".into()),
        ];
        let mut spec = MonitorSpec {
            key: "api".parse().unwrap(),
            name: "API".into(),
            check: CheckSpec::Http(http),
            policy: CheckPolicy::default(),
            active: true,
            tags: vec![],
            group: None,
        };

        redact(&mut spec);

        let json = serde_json::to_string(&spec).unwrap();
        for secret in ["hunter2", "sekret", "bob", "text/html"] {
            assert!(!json.contains(secret), "{json}");
        }
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
