//! Admin authentication: GitHub OAuth (PKCE), the allowlist, and sessions.
//!
//! Sessions are Topcoat session tokens; only their SHA-256 is stored. Every
//! request re-checks the allowlist, so removing a username from the
//! configuration revokes that admin's sessions on the next restart.

mod flow;
mod github;

use std::{fmt, time::Duration};

use jiff::{SignedDuration, Timestamp};
pub use topcoat::cookie::Key;
use topcoat::{
    Result,
    context::{Cx, app_context},
    cookie::{Cookie, Cookies as _, SameSite, private_cookies},
    router::{
        error::{RouterErrorExt as _, SeeOther, not_found, redirect, see_other},
        page, query_params,
        request::headers,
        route,
    },
    session,
    view::{View, view},
};
use uptime_domain::{Allowlist, authorize};
use uptime_store::{AdminUser, GithubIdentity, NewSession};
use url::Url;

pub use github::{GithubOAuth, OAuthError};

use self::flow::Flow;
use crate::{AppState, views::document};

const FLOW_COOKIE: &str = "oauth_flow";
const FLOW_TTL: Duration = Duration::from_secs(10 * 60);
/// Sessions seen more recently than this are not re-extended (saves writes).
const REFRESH_AFTER: SignedDuration = SignedDuration::from_hours(1);

/// How admins sign in.
pub struct AuthSettings {
    pub allowlist: Allowlist,
    pub github: Option<GithubOAuth>,
    /// Debug builds only: a username that can sign in without GitHub.
    pub dev_login: Option<String>,
    /// Inactivity limit (the cookie lifetime, slid forward while in use).
    pub session_idle: Duration,
    /// Absolute limit, however active the session.
    pub session_max_age: Duration,
    /// Encrypts the OAuth flow cookie. Share it between instances.
    pub cookie_key: Key,
}

impl AuthSettings {
    pub fn new(allowlist: Allowlist, cookie_key: Key) -> Self {
        Self {
            allowlist,
            github: None,
            dev_login: None,
            session_idle: Duration::from_secs(7 * 24 * 60 * 60),
            session_max_age: Duration::from_secs(30 * 24 * 60 * 60),
            cookie_key,
        }
    }

    /// Nobody can sign in.
    pub(crate) fn disabled() -> Self {
        Self::new(Allowlist::default(), Key::generate())
    }

    #[must_use]
    pub fn with_github(self, github: GithubOAuth) -> Self {
        Self {
            github: Some(github),
            ..self
        }
    }

    #[must_use]
    pub fn with_dev_login(self, login: Option<String>) -> Self {
        Self {
            dev_login: login,
            ..self
        }
    }

    #[must_use]
    pub fn with_session_limits(self, idle: Duration, max_age: Duration) -> Self {
        Self {
            session_idle: idle,
            session_max_age: max_age,
            ..self
        }
    }

    fn dev_login_enabled(&self) -> Option<&str> {
        self.dev_login.as_deref().filter(|_| cfg!(debug_assertions))
    }
}

impl fmt::Debug for AuthSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthSettings")
            .field("allowlist", &self.allowlist.to_string())
            .field("github", &self.github.is_some())
            .field("dev_login", &self.dev_login)
            .field("session_idle", &self.session_idle)
            .field("session_max_age", &self.session_max_age)
            .finish_non_exhaustive()
    }
}

pub(crate) fn app(cx: &Cx) -> &AppState {
    app_context::<AppState>(cx)
}

/// The signed-in admin, if the request carries a valid session for an
/// allowlisted user.
pub(crate) async fn current_admin(cx: &Cx) -> Result<Option<AdminUser>> {
    let Some(hash) = session::token_hash(cx).await? else {
        return Ok(None);
    };
    let state = app(cx);
    let now = Timestamp::now();
    let max_age = SignedDuration::try_from(state.auth.session_max_age)?;
    let Some((admin, info)) = state.store.session_admin(&hash, now, max_age).await? else {
        return Ok(None);
    };
    if !state.auth.allowlist.contains(&admin.login) {
        return Ok(None);
    }
    if now.duration_since(info.last_seen_at) > REFRESH_AFTER
        && let Some(refreshed) = session::refresh(cx).await?
    {
        state
            .store
            .extend_session(&refreshed.token_hash, timestamp(refreshed.expires_at), now)
            .await?;
    }
    Ok(Some(admin))
}

/// The signed-in admin, or a redirect to the login page.
pub(crate) async fn require_admin(cx: &Cx) -> Result<AdminUser> {
    Ok(current_admin(cx).await?.ok_or_redirect("/login")?)
}

fn to_login(error: &str) -> SeeOther {
    see_other(format!("/login?error={error}"))
}

fn callback_url(state: &AppState) -> Result<Url> {
    Ok(state.hosts.app_url().join("/auth/github/callback")?)
}

fn timestamp(at: std::time::SystemTime) -> Timestamp {
    Timestamp::try_from(at).unwrap_or_else(|_| Timestamp::now())
}

// ── Pages and routes ─────────────────────────────────────────────────────

#[query_params]
pub(crate) struct LoginQuery {
    error: Option<String>,
}

/// Also accepts POST: a form submitted with an expired session is redirected
/// here with 307, which keeps the method.
#[page([GET, POST] "/login")]
pub(crate) async fn login_page(cx: &Cx) -> Result<impl View> {
    if current_admin(cx).await?.is_some() {
        return Err(redirect("/admin").into());
    }
    let state = app(cx);
    let message = query_params::<LoginQuery>(cx)
        .ok()
        .and_then(|query| query.error.as_deref())
        .and_then(|error| match error {
            "denied" => Some("That GitHub account isn't on the admin list."),
            "expired" => Some("The sign-in attempt expired. Please try again."),
            "github" => Some("GitHub didn't complete the sign-in. Please try again."),
            _ => None,
        });
    let github = state.auth.github.is_some();
    let dev_account = state.auth.dev_login_enabled().map(str::to_owned);
    Ok(view! {
        document(title: "Sign in", modules: &["starfield"],
            <div class="center">
                <sb-starfield speed="6" density="220" tint="violet" label=""></sb-starfield>
                <div class="card login stack">
                    <h1>"uptime"<em>"status"</em></h1>
                    <p class="muted">"Sign in to manage monitors and status pages."</p>
                    if let Some(message) = message {
                        <p class="flash flash-error" role="alert">(message)</p>
                    }
                    if github {
                        <a class="btn btn-pixel" href="/auth/github">"Sign in with GitHub"</a>
                    } else {
                        <p class="muted small">"GitHub sign-in is not configured."</p>
                    }
                    if let Some(login) = dev_account {
                        <form method="post" action="/auth/dev">
                            <button class="btn" type="submit">"Development sign-in as " (login)</button>
                        </form>
                    }
                </div>
            </div>
        )
    })
}

/// Starts the GitHub login: remember state + verifier, then redirect.
#[route(GET "/auth/github")]
pub(crate) async fn github_start(cx: &Cx) -> Result<SeeOther> {
    let state = app(cx);
    let Some(github) = &state.auth.github else {
        return Ok(to_login("github"));
    };
    let flow = Flow::new();
    let max_age = topcoat::cookie::time::Duration::try_from(FLOW_TTL)?;
    private_cookies(cx).add(
        Cookie::build((FLOW_COOKIE, flow.encode()))
            .path("/auth")
            .http_only(true)
            .secure(true)
            .same_site(SameSite::Lax)
            .max_age(max_age)
            .build(),
    );
    Ok(see_other(github.authorize_url(
        &callback_url(state)?,
        &flow.state,
        &flow.challenge(),
    )))
}

#[query_params]
pub(crate) struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
}

/// GitHub sends the browser back here with `code` and `state`.
#[route(GET "/auth/github/callback")]
pub(crate) async fn github_callback(cx: &Cx) -> Result<SeeOther> {
    let jar = private_cookies(cx);
    let flow = jar
        .get(FLOW_COOKIE)
        .and_then(|cookie| Flow::decode(cookie.value()));
    jar.remove(Cookie::build(FLOW_COOKIE).path("/auth").build());

    let state = app(cx);
    let Some(github) = &state.auth.github else {
        return Ok(to_login("github"));
    };
    let query = query_params::<CallbackQuery>(cx).ok();
    let (Some(flow), Some(code), Some(returned)) = (
        flow,
        query.and_then(|q| q.code.as_deref()),
        query.and_then(|q| q.state.as_deref()),
    ) else {
        return Ok(to_login("expired"));
    };
    if !flow.matches(returned) {
        return Ok(to_login("expired"));
    }
    match github
        .exchange(code, &flow.verifier, &callback_url(state)?)
        .await
    {
        Ok(identity) => sign_in(cx, &identity).await,
        Err(error) => {
            tracing::warn!(error = %uptime_domain::Report(&error), "GitHub login failed");
            Ok(to_login("github"))
        }
    }
}

/// Checks the allowlist and pinning, records the login and starts a session.
async fn sign_in(cx: &Cx, identity: &GithubIdentity) -> Result<SeeOther> {
    let state = app(cx);
    let pinned = state.store.pinned_github_id(&identity.login).await?;
    if let Err(denied) = authorize(
        &state.auth.allowlist,
        &identity.login,
        identity.github_id,
        pinned,
    ) {
        tracing::warn!(%denied, github_id = identity.github_id, "admin login denied");
        return Ok(to_login("denied"));
    }
    let now = Timestamp::now();
    let admin = state.store.record_admin_login(identity, now).await?;
    let started = session::start(cx).await?;
    let user_agent = headers(cx)
        .get("user-agent")
        .and_then(|value| value.to_str().ok())
        .map(|ua| ua.chars().take(200).collect());
    state
        .store
        .create_session(&NewSession {
            token_hash: *started.token_hash,
            user_id: admin.id,
            created_at: now,
            expires_at: timestamp(started.expires_at),
            user_agent,
        })
        .await?;
    tracing::info!(login = %admin.login, "admin signed in");
    Ok(see_other("/admin"))
}

#[route(POST "/auth/logout")]
pub(crate) async fn logout(cx: &Cx) -> Result<SeeOther> {
    if let Some(hash) = session::stop(cx).await? {
        app(cx).store.delete_session(&hash).await?;
    }
    Ok(see_other("/login"))
}

/// Development only: sign in as the configured user without GitHub.
#[route(POST "/auth/dev")]
pub(crate) async fn dev_login(cx: &Cx) -> Result<SeeOther> {
    let Some(login) = app(cx).auth.dev_login_enabled().map(str::to_owned) else {
        return Err(not_found().into());
    };
    // A negative id can never collide with a real GitHub account.
    let github_id = -1
        - i64::from(
            login
                .bytes()
                .fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(u32::from(b))),
        );
    let identity = GithubIdentity {
        github_id,
        login,
        name: Some("Development".into()),
        avatar_url: None,
    };
    sign_in(cx, &identity).await
}
