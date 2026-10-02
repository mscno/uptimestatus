//! GitHub OAuth (authorization code flow with PKCE, no scopes).

use std::{fmt, time::Duration};

use serde::Deserialize;
use uptime_store::GithubIdentity;
use url::Url;

/// Why the OAuth exchange failed.
#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    /// GitHub refused the code (expired, reused, wrong verifier...).
    #[error("GitHub rejected the login: {0}")]
    Rejected(String),
    /// GitHub could not be reached or answered unexpectedly.
    #[error("GitHub is unavailable: {0}")]
    Unavailable(String),
}

/// Talks to GitHub's OAuth and user endpoints.
#[derive(Clone)]
pub struct GithubOAuth {
    client_id: String,
    client_secret: String,
    authorize_url: Url,
    token_url: Url,
    api_url: Url,
    http: reqwest::Client,
}

impl fmt::Debug for GithubOAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GithubOAuth")
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

impl GithubOAuth {
    pub fn new(client_id: impl Into<String>, client_secret: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!("uptimestatus/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        Self {
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            authorize_url: Self::parse("https://github.com/login/oauth/authorize"),
            token_url: Self::parse("https://github.com/login/oauth/access_token"),
            api_url: Self::parse("https://api.github.com/"),
            http,
        }
    }

    #[allow(clippy::expect_used)] // literal URLs
    fn parse(url: &str) -> Url {
        url.parse().expect("valid built-in GitHub URL")
    }

    /// Points the client at other endpoints (tests, GitHub Enterprise).
    pub fn with_endpoints(self, authorize_url: Url, token_url: Url, api_url: Url) -> Self {
        Self {
            authorize_url,
            token_url,
            api_url,
            ..self
        }
    }

    /// Where to send the browser to start the login.
    pub fn authorize_url(&self, redirect_uri: &Url, state: &str, code_challenge: &str) -> Url {
        let mut url = self.authorize_url.clone();
        url.query_pairs_mut()
            .append_pair("client_id", &self.client_id)
            .append_pair("redirect_uri", redirect_uri.as_str())
            .append_pair("scope", "")
            .append_pair("state", state)
            .append_pair("code_challenge", code_challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("allow_signup", "false");
        url
    }

    /// Trades the callback `code` for the signed-in GitHub identity.
    pub async fn exchange(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &Url,
    ) -> Result<GithubIdentity, OAuthError> {
        let token: TokenResponse = self
            .http
            .post(self.token_url.clone())
            .header(reqwest::header::ACCEPT, "application/json")
            .form(&[
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("code", code),
                ("code_verifier", verifier),
                ("redirect_uri", redirect_uri.as_str()),
            ])
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(unavailable)?
            .json()
            .await
            .map_err(unavailable)?;
        let access_token = match token {
            TokenResponse {
                access_token: Some(token),
                ..
            } => token,
            TokenResponse {
                error,
                error_description,
                ..
            } => {
                let reason = [error, error_description]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(": ");
                return Err(OAuthError::Rejected(if reason.is_empty() {
                    "no access token".into()
                } else {
                    reason
                }));
            }
        };

        let user_url = self
            .api_url
            .join("user")
            .map_err(|e| OAuthError::Unavailable(e.to_string()))?;
        let user: UserResponse = self
            .http
            .get(user_url)
            .bearer_auth(access_token)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header("x-github-api-version", "2022-11-28")
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(unavailable)?
            .json()
            .await
            .map_err(unavailable)?;
        Ok(GithubIdentity {
            github_id: user.id,
            login: user.login,
            name: user.name,
            avatar_url: user.avatar_url,
        })
    }
}

fn unavailable(error: reqwest::Error) -> OAuthError {
    OAuthError::Unavailable(error.to_string())
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Deserialize)]
struct UserResponse {
    id: i64,
    login: String,
    name: Option<String>,
    avatar_url: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_debug_hides_client_secret() {
        let oauth = GithubOAuth::new("client-id", "client-secret");
        let shown = format!("{oauth:?}");
        assert!(!shown.contains("client-secret"), "{shown}");
    }
}
