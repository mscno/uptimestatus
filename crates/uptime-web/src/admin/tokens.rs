//! API tokens for the JSON API: create (the secret is shown once) and revoke.

use jiff::Timestamp;
use serde::Deserialize;
use topcoat::{
    Result,
    context::Cx,
    router::{
        content::{Form, Html},
        error::{SeeOther, not_found, see_other},
        path_param,
        response::{IntoResponse as _, Response},
        route,
    },
    view::{ViewExt as _, view},
};
use uptime_domain::TokenScope;
use uptime_store::{AdminUser, ApiToken, NewApiToken};

use super::Id;
use crate::{
    api_v1::{hash_token, new_token},
    auth::{app, require_admin},
    views::{admin_shell, ago, delete_button},
};

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct TokenForm {
    name: String,
    scope: String,
}

/// The whole page: existing tokens, the create form, and (right after
/// creating) the new secret.
async fn render(
    cx: &Cx,
    admin: &AdminUser,
    tokens: &[ApiToken],
    secret: Option<&str>,
    error: Option<&str>,
) -> Result<String> {
    let now = Timestamp::now();
    let rows: Vec<(String, &ApiToken)> = tokens
        .iter()
        .map(|t| (format!("/admin/api/tokens/{}/delete", t.id), t))
        .collect();
    Ok(view! { cx =>
        admin_shell(title: "API", admin: admin, section: "api",
            <div class="page-head"><h1>"API"</h1></div>
            <p class="muted">
                "Automate with the JSON API at " <code>"/api/v1"</code> ": send "
                <code>"Authorization: Bearer <token>"</code> ". "
                "Read-only tokens see secrets redacted."
            </p>
            if let Some(secret) = secret {
                <div class="card">
                    <p><strong>"Copy your new token now. It is not shown again."</strong></p>
                    <div class="row small">
                        <code id="new-token">(secret)</code>
                        <sb-copy-button value=(secret) label="Copy token"></sb-copy-button>
                    </div>
                </div>
            }
            <div class="card">
                <h2>"New token"</h2>
                if let Some(error) = error {
                    <p class="flash flash-error" role="alert">(error)</p>
                }
                <form method="post" action="/admin/api/tokens">
                    <div class="fields">
                        <div class="field">
                            <label for="field-name">"Name"</label>
                            <input type="text" id="field-name" name="name" placeholder="terraform">
                        </div>
                        <div class="field">
                            <label for="field-scope">"Access"</label>
                            <select id="field-scope" name="scope">
                                for scope in TokenScope::ALL {
                                    <option value=(scope.as_str())>(scope.label())</option>
                                }
                            </select>
                        </div>
                    </div>
                    <div class="actions"><button class="btn btn-primary" type="submit">"Create token"</button></div>
                </form>
            </div>
            if tokens.is_empty() {
                <div class="card empty"><p>"No tokens yet."</p></div>
            } else {
                <div class="card table-wrap">
                    <table class="list">
                        <thead><tr><th>"Name"</th><th>"Access"</th><th class="hide-sm">"Created by"</th><th class="hide-sm">"Last used"</th><th></th></tr></thead>
                        <tbody>
                            for (delete_url, token) in &rows {
                                <tr>
                                    <td class="name-cell">(token.name.as_str())</td>
                                    <td>(token.scope.label())</td>
                                    <td class="small hide-sm">(token.created_by.as_str()) " · " ago(at: token.created_at, now: now)</td>
                                    <td class="small hide-sm">
                                        if let Some(at) = token.last_used_at { ago(at: at, now: now) } else { "never" }
                                    </td>
                                    <td class="row-actions">
                                        delete_button(action: delete_url,
                                            heading: "Revoke this token?",
                                            body: "Anything using it stops working immediately.")
                                    </td>
                                </tr>
                            }
                        </tbody>
                    </table>
                </div>
            }
        )
    }
    .single()
    .await?
    .render(cx))
}

#[route(GET "/admin/api")]
pub(crate) async fn api_page(cx: &Cx) -> Result<Response> {
    let admin = require_admin(cx).await?;
    let tokens = app(cx).store.list_api_tokens().await?;
    Html(render(cx, &admin, &tokens, None, None).await?).into_response(cx)
}

#[route(POST "/admin/api/tokens")]
pub(crate) async fn create_token(cx: &Cx, Form(input): Form<TokenForm>) -> Result<Response> {
    let admin = require_admin(cx).await?;
    let state = app(cx);
    let name = input.name.trim();
    let scope = input.scope.parse::<TokenScope>();
    let problem = match (&scope, name.is_empty()) {
        (_, true) => Some("Give the token a name."),
        (Err(_), _) => Some("Choose an access level."),
        _ => None,
    };
    if let Some(problem) = problem {
        let tokens = state.store.list_api_tokens().await?;
        return (
            topcoat::router::StatusCode::UNPROCESSABLE_ENTITY,
            Html(render(cx, &admin, &tokens, None, Some(problem)).await?),
        )
            .into_response(cx);
    }
    let secret = new_token();
    state
        .store
        .create_api_token(
            &NewApiToken {
                name: name.to_owned(),
                token_hash: hash_token(&secret),
                scope: scope.unwrap_or(TokenScope::Read),
                created_by: admin.login.clone(),
            },
            Timestamp::now(),
        )
        .await?;
    tracing::info!(name, by = %admin.login, "API token created");
    let tokens = state.store.list_api_tokens().await?;
    Html(render(cx, &admin, &tokens, Some(&secret), None).await?).into_response(cx)
}

#[route(POST "/admin/api/tokens/{id}/delete")]
pub(crate) async fn revoke_token(cx: &Cx) -> Result<SeeOther> {
    let admin = require_admin(cx).await?;
    let id = *path_param::<Id>(cx)?;
    if !app(cx).store.delete_api_token(id).await? {
        return Err(not_found().into());
    }
    tracing::info!(token = id, by = %admin.login, "API token revoked");
    Ok(see_other("/admin/api"))
}
