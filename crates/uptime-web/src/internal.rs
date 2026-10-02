//! The internal listener (`INTERNAL_PORT`): health and readiness for the
//! platform. Never expose it publicly.

use axum::{Router, extract::State, http::StatusCode, routing::get};

use crate::AppState;

pub(crate) fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .with_state(state)
}

/// Liveness: the process is serving HTTP.
pub(crate) async fn healthz() -> &'static str {
    "ok"
}

/// Readiness: the database answers.
async fn readyz(State(state): State<AppState>) -> (StatusCode, &'static str) {
    match state.store.ping().await {
        Ok(()) => (StatusCode::OK, "ready"),
        Err(error) => {
            tracing::warn!(error = %uptime_domain::Report(&error), "readiness check failed");
            (StatusCode::SERVICE_UNAVAILABLE, "database unavailable")
        }
    }
}
