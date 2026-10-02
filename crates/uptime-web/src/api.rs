//! Machine endpoints: push monitor heartbeats.
//!
//! `GET|POST /api/push/{token}?status=up|down&msg=…&ping=<ms>`: the token is
//! the secret; any other status than `down` counts as up.

use jiff::Timestamp;
use serde::Serialize;
use topcoat::{
    Result,
    context::Cx,
    router::{
        StatusCode,
        content::Json,
        path_param, query_params,
        response::{IntoResponse as _, Response},
        route,
    },
};
use uptime_domain::Push;

use crate::auth::app;

path_param!(token);

#[query_params]
pub(crate) struct PushQuery {
    status: Option<String>,
    msg: Option<String>,
    ping: Option<String>,
}

#[derive(Serialize)]
struct PushReply {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    msg: Option<&'static str>,
}

/// Longest message kept from a push.
const MAX_MESSAGE: usize = 500;

#[route([GET, POST] "/api/push/{token}")]
pub(crate) async fn push(cx: &Cx) -> Result<Response> {
    let token = path_param::<Token>(cx);
    let query = query_params::<PushQuery>(cx).ok();
    let status = query.and_then(|q| q.status.as_deref()).unwrap_or("up");
    let push = Push {
        at: Timestamp::now(),
        up: !status.eq_ignore_ascii_case("down"),
        message: query
            .and_then(|q| q.msg.as_deref())
            .map(|m| m.trim().chars().take(MAX_MESSAGE).collect::<String>())
            .filter(|m| !m.is_empty()),
        ping: query
            .and_then(|q| q.ping.as_deref())
            .and_then(|p| p.trim().parse::<f64>().ok())
            .filter(|ms| ms.is_finite() && *ms >= 0.0)
            .map(|ms| std::time::Duration::from_secs_f64(ms / 1000.0)),
    };
    let state = app(cx);
    match state.store.record_push(token, &push).await? {
        Some(monitor) => {
            state.wake_scheduler();
            tracing::debug!(%monitor, up = push.up, "push received");
            Json(PushReply {
                ok: true,
                msg: None,
            })
            .into_response(cx)
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(PushReply {
                ok: false,
                msg: Some("unknown push token"),
            }),
        )
            .into_response(cx),
    }
}
