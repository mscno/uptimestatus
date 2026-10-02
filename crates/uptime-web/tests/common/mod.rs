//! Test harness: a real database, a fake GitHub, and a cookie-aware client.

#![allow(dead_code, unreachable_pub, clippy::unwrap_used, clippy::expect_used)]

use std::{collections::HashMap, sync::Arc, time::Duration};

use axum::{
    Router,
    body::Body,
    http::{Request, Response, StatusCode, header},
};
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;
use uptime_domain::Allowlist;
use uptime_probe::{AddressPolicy, Prober, ProberConfig};
use uptime_runtime::EventBus;
use uptime_testkit::TestDb;
use uptime_web::{
    AppState, DomainCache, Hosts,
    auth::{AuthSettings, GithubOAuth, Key},
};
use url::Url;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

pub const APP_HOST: &str = "status.example.com";

/// A fresh directory for one test app's uploads.
pub fn media_dir() -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("uptimestatus-media-{}-{n}", std::process::id()))
}

pub struct TestApp {
    pub db: TestDb,
    pub github: MockServer,
    pub state: AppState,
}

impl TestApp {
    pub async fn new() -> Self {
        Self::with_allowlist("alice, bob").await
    }

    pub async fn with_allowlist(allowlist: &str) -> Self {
        let db = TestDb::new().await;
        let github = MockServer::start().await;
        let state = Self::state(&db, &github, allowlist, Key::generate());
        Self { db, github, state }
    }

    pub fn state(db: &TestDb, github: &MockServer, allowlist: &str, key: Key) -> AppState {
        let base: Url = github.uri().parse().unwrap();
        let oauth = GithubOAuth::new("client", "secret").with_endpoints(
            base.join("/login/oauth/authorize").unwrap(),
            base.join("/login/oauth/access_token").unwrap(),
            base.join("/api/").unwrap(),
        );
        let auth =
            AuthSettings::new(allowlist.parse::<Allowlist>().unwrap(), key).with_github(oauth);
        let prober = Prober::new(ProberConfig {
            policy: AddressPolicy::ALLOW_ALL,
            ..ProberConfig::default()
        })
        .unwrap();
        AppState::new(
            db.store().clone(),
            Hosts::new(
                format!("https://{APP_HOST}").parse().unwrap(),
                "edge.example.com",
            ),
        )
        .with_domains(DomainCache::default())
        .with_auth(auth)
        .with_events(EventBus::default())
        .with_prober(Arc::new(prober))
        .with_media(uptime_web::media::MediaStore::open(&media_dir()).unwrap())
        // Unguarded: test receivers listen on localhost.
        .with_sender(
            uptime_notify::Sender::new(reqwest::Client::new())
                .with_app_url(format!("https://{APP_HOST}").parse().unwrap()),
        )
    }

    pub fn client(&self) -> Client {
        Client {
            app: uptime_web::public_router(self.state.clone()),
            cookies: HashMap::new(),
        }
    }

    /// Makes the fake GitHub sign in `login` (GitHub user `github_id`) on the next exchange.
    pub async fn github_signs_in(&self, login: &str, github_id: i64) {
        self.github.reset().await;
        Mock::given(method("POST"))
            .and(path("/login/oauth/access_token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "access_token": "token" })),
            )
            .mount(&self.github)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/user"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "login": login, "id": github_id, "name": format!("{login} (test)"), "avatar_url": null
            })))
            .mount(&self.github)
            .await;
    }

    /// A client signed in as `login` through the full OAuth flow.
    pub async fn signed_in(&self, login: &str, github_id: i64) -> Client {
        self.github_signs_in(login, github_id).await;
        let mut client = self.client();
        let outcome = client.oauth_round_trip().await;
        assert_eq!(
            outcome.location(),
            Some("/admin"),
            "login as {login} failed: {outcome:?}"
        );
        client
    }
}

/// What came back from the app.
#[derive(Debug)]
pub struct Reply {
    pub status: StatusCode,
    pub headers: axum::http::HeaderMap,
    pub body: String,
}

impl Reply {
    pub fn location(&self) -> Option<&str> {
        self.headers
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
    }

    pub fn assert_contains(&self, needle: &str) -> &Self {
        assert!(
            self.body.contains(needle),
            "expected {needle:?} in body:\n{}",
            self.body
        );
        self
    }
}

/// A browser-ish client: keeps cookies, never follows redirects.
pub struct Client {
    app: Router,
    pub cookies: HashMap<String, String>,
}

impl Client {
    /// A client for `state` carrying existing cookies (e.g. after a restart).
    pub fn with_state(state: AppState, cookies: HashMap<String, String>) -> Self {
        Self {
            app: uptime_web::public_router(state),
            cookies,
        }
    }

    pub async fn get(&mut self, uri: &str) -> Reply {
        self.send(Request::get(uri), Body::empty()).await
    }

    /// A GET for another hostname (e.g. a custom domain).
    pub async fn get_on(&mut self, host: &str, uri: &str) -> Reply {
        let request = Request::get(uri)
            .header(header::HOST, host)
            .body(Body::empty())
            .unwrap();
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            headers,
            body: String::from_utf8_lossy(&body).into_owned(),
        }
    }

    pub async fn post_form(&mut self, uri: &str, fields: &[(&str, &str)]) -> Reply {
        let body = serde_urlencoded::to_string(fields).unwrap();
        let request =
            Request::post(uri).header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        self.send(request, Body::from(body)).await
    }

    pub async fn post_form_from(
        &mut self,
        origin: &str,
        uri: &str,
        fields: &[(&str, &str)],
    ) -> Reply {
        let body = serde_urlencoded::to_string(fields).unwrap();
        let request = Request::post(uri)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::ORIGIN, origin);
        self.send(request, Body::from(body)).await
    }

    /// A `multipart/form-data` upload of one file field.
    pub async fn post_file(
        &mut self,
        uri: &str,
        field: &str,
        file_name: &str,
        bytes: &[u8],
    ) -> Reply {
        let boundary = "uptimestatus-test-boundary";
        let mut body = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"{file_name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
        .into_bytes();
        body.extend_from_slice(bytes);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let request = Request::post(uri).header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        );
        self.send(request, Body::from(body)).await
    }

    /// A Datastar `@post(..., {contentType: 'form'})` request.
    pub async fn datastar_form(&mut self, uri: &str, fields: &[(&str, &str)]) -> Reply {
        let body = serde_urlencoded::to_string(fields).unwrap();
        let request = Request::post(uri)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header("datastar-request", "true");
        self.send(request, Body::from(body)).await
    }

    /// A Datastar `@post(...)` request: the signals go as a JSON body.
    pub async fn datastar_signals(&mut self, uri: &str, signals: &serde_json::Value) -> Reply {
        let request = Request::post(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .header("datastar-request", "true");
        self.send(request, Body::from(signals.to_string())).await
    }

    /// A JSON API call: `method` on `uri`, optionally with a bearer token and a JSON body.
    pub async fn api(
        &mut self,
        method: &str,
        uri: &str,
        token: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> Reply {
        let mut request = Request::builder().method(method).uri(uri);
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let body = match body {
            Some(json) => {
                request = request.header(header::CONTENT_TYPE, "application/json");
                Body::from(json.to_string())
            }
            None => Body::empty(),
        };
        self.send(request, body).await
    }

    /// Opens a streaming GET and returns the response (body unread).
    pub async fn open_stream(&mut self, uri: &str) -> Response<Body> {
        let request = self
            .prepare(Request::get(uri).header("datastar-request", "true"))
            .body(Body::empty())
            .unwrap();
        self.app.clone().oneshot(request).await.unwrap()
    }

    /// Starts the GitHub login, then completes the callback with the issued state.
    pub async fn oauth_round_trip(&mut self) -> Reply {
        let start = self.get("/auth/github").await;
        assert_eq!(start.status, StatusCode::SEE_OTHER, "{start:?}");
        let authorize: Url = start
            .location()
            .expect("redirect to GitHub")
            .parse()
            .unwrap();
        let state = authorize
            .query_pairs()
            .find(|(k, _)| k == "state")
            .expect("state")
            .1
            .into_owned();
        self.get(&format!(
            "/auth/github/callback?code=the-code&state={state}"
        ))
        .await
    }

    fn prepare(&self, builder: axum::http::request::Builder) -> axum::http::request::Builder {
        let cookie = self
            .cookies
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ");
        let builder = builder.header(header::HOST, APP_HOST);
        if cookie.is_empty() {
            builder
        } else {
            builder.header(header::COOKIE, cookie)
        }
    }

    async fn send(&mut self, builder: axum::http::request::Builder, body: Body) -> Reply {
        let request = self.prepare(builder).body(body).unwrap();
        let response =
            tokio::time::timeout(Duration::from_secs(30), self.app.clone().oneshot(request))
                .await
                .expect("app answered")
                .unwrap();
        for set_cookie in response.headers().get_all(header::SET_COOKIE) {
            let raw = set_cookie.to_str().unwrap();
            let (pair, attributes) = raw.split_once(';').unwrap_or((raw, ""));
            let (name, value) = pair.split_once('=').unwrap();
            let removed = attributes.to_ascii_lowercase().contains("max-age=0") || value.is_empty();
            if removed {
                self.cookies.remove(name.trim());
            } else {
                self.cookies
                    .insert(name.trim().to_owned(), value.trim().to_owned());
            }
        }
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            headers,
            body: String::from_utf8_lossy(&body).into_owned(),
        }
    }
}

/// Reads SSE frames until `needle` shows up (or times out).
pub async fn read_stream_until(response: Response<Body>, needle: &str) -> String {
    let mut body = response.into_body();
    let mut seen = String::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(frame) = body.frame().await {
            if let Ok(data) = frame.unwrap().into_data() {
                seen.push_str(&String::from_utf8_lossy(&data));
                if seen.contains(needle) {
                    return;
                }
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("stream never contained {needle:?}; saw:\n{seen}"));
    seen
}
