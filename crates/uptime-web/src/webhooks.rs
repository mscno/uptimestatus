//! `POST /webhooks/lettermint`: Lettermint's signed delivery events.
//!
//! Hard bounces, spam complaints and suppressions stop all email to the
//! address (on every page); unsubscribes through the mail provider end the
//! address's email subscriptions. Answers 404 unless a webhook secret is
//! configured, 401 for deliveries that fail verification.

use axum::{
    body::Bytes,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse as _, Response},
};
use jiff::Timestamp;
use serde_json::{Value, json};
use uptime_domain::EmailAddress;
use uptime_store::Subscriber;

use crate::{AppState, audit};

/// What a delivery asks for.
enum Action {
    Suppress,
    Unsubscribe,
}

fn action(event: &str, data: &Value) -> Option<(Action, String)> {
    let recipient = || data["recipient"].as_str().map(str::to_owned);
    match event {
        "message.hard_bounced" | "message.spam_complaint" | "message.suppressed" => {
            Some((Action::Suppress, recipient()?))
        }
        "message.unsubscribed" => Some((Action::Unsubscribe, recipient()?)),
        "suppression.added" if data["type"] == "email" => {
            Some((Action::Suppress, data["value"].as_str()?.to_owned()))
        }
        _ => None,
    }
}

pub(crate) async fn lettermint(state: AppState, headers: HeaderMap, body: Bytes) -> Response {
    let Some(secret) = state.lettermint_webhook_secret.as_deref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let (event, data) = match uptime_notify::verify_webhook(
        secret,
        &body,
        header("x-lettermint-signature"),
        header("x-lettermint-delivery"),
    ) {
        Ok(verified) => verified,
        Err(error) => {
            tracing::warn!(error = %error, "rejected a Lettermint webhook delivery");
            return StatusCode::UNAUTHORIZED.into_response();
        }
    };
    let Some((action, address)) = action(&event, &data) else {
        return StatusCode::NO_CONTENT.into_response();
    };
    let Ok(address) = address.parse::<EmailAddress>() else {
        return StatusCode::NO_CONTENT.into_response();
    };
    match apply(&state, &event, action, &address).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => {
            tracing::error!(error = %uptime_domain::Report(&*error), "handling a Lettermint webhook failed");
            // Lettermint retries failed deliveries.
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn apply(
    state: &AppState,
    event: &str,
    action: Action,
    address: &EmailAddress,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let now = Timestamp::now();
    let (changed, verb, past): (Vec<Subscriber>, &str, &str) = match action {
        Action::Suppress => (
            state.store.suppress_email(address, now).await?,
            "subscriber.suppress",
            "Suppressed",
        ),
        Action::Unsubscribe => (
            state.store.unsubscribe_email(address, now).await?,
            "subscriber.unsubscribe",
            "Unsubscribed",
        ),
    };
    for subscriber in &changed {
        audit::record(
            state,
            "system",
            verb,
            audit::target("page", subscriber.page_id),
            format!(
                "{past} {} after a Lettermint {event} event",
                address.masked()
            ),
            json!({ "subscriber": subscriber.id, "event": event }),
        )
        .await
        .map_err(|e| e.to_string())?;
    }
    if !changed.is_empty() {
        tracing::info!(
            event,
            count = changed.len(),
            "subscriptions changed by Lettermint"
        );
    }
    Ok(())
}
