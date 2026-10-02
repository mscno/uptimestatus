#![allow(clippy::unwrap_used, clippy::expect_used)] // tests fail loudly

//! The GitHub OAuth client against a fake GitHub.

use pretty_assertions::assert_eq;
use uptime_store::GithubIdentity;
use uptime_web::auth::{GithubOAuth, OAuthError};
use url::Url;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_string_contains, header, method, path},
};

fn client(server: &MockServer) -> GithubOAuth {
    let base: Url = server.uri().parse().unwrap();
    GithubOAuth::new("client-123", "secret-456").with_endpoints(
        base.join("/login/oauth/authorize").unwrap(),
        base.join("/login/oauth/access_token").unwrap(),
        base.join("/api/").unwrap(),
    )
}

fn redirect_uri() -> Url {
    "https://status.example.com/auth/github/callback"
        .parse()
        .unwrap()
}

#[tokio::test]
async fn authorize_url_carries_state_and_pkce_challenge() {
    let server = MockServer::start().await;
    let url = client(&server).authorize_url(&redirect_uri(), "state-abc", "challenge-xyz");

    let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(url.path(), "/login/oauth/authorize");
    assert_eq!(query["client_id"], "client-123");
    assert_eq!(query["redirect_uri"], redirect_uri().as_str());
    assert_eq!(query["state"], "state-abc");
    assert_eq!(query["code_challenge"], "challenge-xyz");
    assert_eq!(query["code_challenge_method"], "S256");
    assert_eq!(query["scope"], "", "no scopes: public profile only");
    assert_eq!(query["allow_signup"], "false");
}

#[tokio::test]
async fn exchanges_the_code_and_fetches_the_user() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/login/oauth/access_token"))
        .and(header("accept", "application/json"))
        .and(body_string_contains("client_id=client-123"))
        .and(body_string_contains("client_secret=secret-456"))
        .and(body_string_contains("code=code-1"))
        .and(body_string_contains("code_verifier=verifier-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "gho_token", "token_type": "bearer", "scope": ""
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/user"))
        .and(header("authorization", "Bearer gho_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "login": "Octocat", "id": 583231, "name": "The Octocat",
            "avatar_url": "https://avatars.example/583231", "email": null
        })))
        .expect(1)
        .mount(&server)
        .await;

    let identity = client(&server)
        .exchange("code-1", "verifier-1", &redirect_uri())
        .await
        .unwrap();

    assert_eq!(
        identity,
        GithubIdentity {
            github_id: 583231,
            login: "Octocat".into(),
            name: Some("The Octocat".into()),
            avatar_url: Some("https://avatars.example/583231".into()),
        }
    );
}

#[tokio::test]
async fn a_rejected_code_is_reported() {
    let server = MockServer::start().await;
    // GitHub reports OAuth errors with 200 OK and an `error` field.
    Mock::given(path("/login/oauth/access_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "error": "bad_verification_code",
            "error_description": "The code passed is incorrect or expired."
        })))
        .mount(&server)
        .await;

    let error = client(&server)
        .exchange("stale", "v", &redirect_uri())
        .await
        .unwrap_err();

    assert!(
        matches!(&error, OAuthError::Rejected(reason) if reason.contains("bad_verification_code")),
        "{error:?}"
    );
}

#[tokio::test]
async fn github_outages_are_reported() {
    let server = MockServer::start().await;
    Mock::given(path("/login/oauth/access_token"))
        .respond_with(ResponseTemplate::new(502))
        .mount(&server)
        .await;

    let error = client(&server)
        .exchange("c", "v", &redirect_uri())
        .await
        .unwrap_err();

    assert!(matches!(error, OAuthError::Unavailable(_)), "{error:?}");
}
