//! OAuth 2.0 flows against a local authorization server.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use std::sync::Mutex;

use serde_json::json;

use merpati_auth::{AuthContext, oauth2};
use merpati_core::tokens::{MemoryTokenStore, OAuth2Token, TokenStore};
use merpati_core::{Auth, AuthType, PreparedRequest};

#[derive(Default)]
struct ServerState {
    challenge: Mutex<Option<String>>,
    token_calls: AtomicUsize,
    last_form: Mutex<HashMap<String, String>>,
}

type Shared = Arc<ServerState>;

async fn authorize(State(s): State<Shared>, Query(q): Query<HashMap<String, String>>) -> Response {
    *s.challenge.lock().unwrap() = q.get("code_challenge").cloned();
    let redirect = q.get("redirect_uri").cloned().unwrap_or_default();
    let state = q.get("state").cloned().unwrap_or_default();
    if q.get("client_id").map(String::as_str) != Some("cid") {
        return Redirect::to(&format!("{redirect}?error=unauthorized_client&state={state}")).into_response();
    }
    Redirect::to(&format!("{redirect}?code=the-code&state={state}")).into_response()
}

fn basic_ok(headers: &HeaderMap) -> bool {
    // base64("cid:secret")
    headers.get("authorization").and_then(|v| v.to_str().ok()) == Some("Basic Y2lkOnNlY3JldA==")
}

async fn token(State(s): State<Shared>, headers: HeaderMap, Form(f): Form<HashMap<String, String>>) -> Response {
    s.token_calls.fetch_add(1, Ordering::SeqCst);
    *s.last_form.lock().unwrap() = f.clone();
    let bad =
        |e: &str| (StatusCode::BAD_REQUEST, Json(json!({"error": e, "error_description": "rejected"}))).into_response();
    match f.get("grant_type").map(String::as_str) {
        Some("client_credentials") => {
            if !basic_ok(&headers) {
                return bad("invalid_client");
            }
            Json(json!({"access_token": "cc-token", "token_type": "Bearer", "expires_in": 3600, "scope": f.get("scope")}))
                .into_response()
        }
        Some("password") => {
            let body_client = f.get("client_id").map(String::as_str) == Some("cid")
                && f.get("client_secret").map(String::as_str) == Some("secret");
            if !body_client {
                return bad("invalid_client");
            }
            if f.get("username").map(String::as_str) != Some("alice")
                || f.get("password").map(String::as_str) != Some("pw")
            {
                return bad("invalid_grant");
            }
            Json(json!({"access_token": "pw-token", "refresh_token": "r1", "expires_in": 3600})).into_response()
        }
        Some("refresh_token") => {
            if f.get("refresh_token").map(String::as_str) != Some("r1") {
                return bad("invalid_grant");
            }
            Json(json!({"access_token": "refreshed-token", "expires_in": 3600})).into_response()
        }
        Some("authorization_code") => {
            if f.get("code").map(String::as_str) != Some("the-code") {
                return bad("invalid_grant");
            }
            let expected = s.challenge.lock().unwrap().clone();
            if let Some(ch) = expected {
                let verifier = f.get("code_verifier").cloned().unwrap_or_default();
                if oauth2::pkce_challenge(&verifier, "S256") != ch {
                    return bad("invalid_grant");
                }
            }
            Json(json!({"access_token": "ac-token", "refresh_token": "r2", "token_type": "bearer", "expires_in": 60}))
                .into_response()
        }
        _ => bad("unsupported_grant_type"),
    }
}

async fn start() -> (String, Shared) {
    let state: Shared = Arc::default();
    let app = Router::new().route("/authorize", get(authorize)).route("/token", post(token)).with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), state)
}

fn client() -> reqwest::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder().redirect(reqwest::redirect::Policy::limited(5)).build().unwrap()
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn base_auth(server: &str, grant: &str) -> Auth {
    Auth::new(AuthType::OAuth2)
        .with("grant_type", grant)
        .with("authUrl", format!("{server}/authorize"))
        .with("accessTokenUrl", format!("{server}/token"))
        .with("clientId", "cid")
        .with("clientSecret", "secret")
        .with("scope", "read write")
}

fn req(auth: Auth, owner: &str) -> PreparedRequest {
    PreparedRequest {
        url: "http://api.test/data".into(),
        auth,
        auth_owner_id: Some(owner.into()),
        ..Default::default()
    }
}

#[tokio::test]
async fn client_credentials_auto_fetch_then_reuse() {
    let (server, state) = start().await;
    let http = client();
    let tokens = MemoryTokenStore::default();
    let ctx = AuthContext { tokens: &tokens, http: &http };

    let mut r = req(base_auth(&server, "client_credentials"), "col-1");
    let applied = merpati_auth::apply(&mut r, &ctx).await.unwrap();
    assert_eq!(r.header("authorization"), Some("Bearer cc-token"));
    assert!(applied.notes.iter().any(|n| n.contains("Fetched")));
    assert_eq!(state.last_form.lock().unwrap().get("scope").map(String::as_str), Some("read write"));
    assert_eq!(tokens.get("col-1").unwrap().scope.as_deref(), Some("read write"));

    // Second request reuses the stored token.
    let mut r = req(base_auth(&server, "client_credentials"), "col-1");
    merpati_auth::apply(&mut r, &ctx).await.unwrap();
    assert_eq!(r.header("authorization"), Some("Bearer cc-token"));
    assert_eq!(state.token_calls.load(Ordering::SeqCst), 1);

    // Wrong client secret: the server error surfaces.
    let mut r = req(base_auth(&server, "client_credentials").with("clientSecret", "nope"), "col-2");
    let err = merpati_auth::apply(&mut r, &ctx).await.unwrap_err();
    assert_eq!(err.to_string(), "OAuth 2.0 server returned `invalid_client`: rejected");
}

#[tokio::test]
async fn password_grant_body_auth_and_refresh() {
    let (server, state) = start().await;
    let http = client();
    let tokens = MemoryTokenStore::default();
    let ctx = AuthContext { tokens: &tokens, http: &http };
    let auth = base_auth(&server, "password_credentials")
        .with("username", "alice")
        .with("password", "pw")
        .with("client_authentication", "body")
        .with("addTokenTo", "queryParams");

    let mut r = req(auth.clone(), "f-1");
    merpati_auth::apply(&mut r, &ctx).await.unwrap();
    assert_eq!(r.url, "http://api.test/data?access_token=pw-token");

    // Expire the stored token: apply refreshes it with the refresh token.
    let mut stored = tokens.get("f-1").unwrap();
    stored.expires_at = Some(0);
    tokens.set("f-1", Some(stored));
    let mut r = req(auth.clone(), "f-1");
    let applied = merpati_auth::apply(&mut r, &ctx).await.unwrap();
    assert_eq!(r.url, "http://api.test/data?access_token=refreshed-token");
    assert!(applied.notes.iter().any(|n| n.contains("Refreshed")));
    let after = tokens.get("f-1").unwrap();
    assert_eq!(after.refresh_token.as_deref(), Some("r1"), "old refresh token kept");
    assert_eq!(state.last_form.lock().unwrap().get("grant_type").map(String::as_str), Some("refresh_token"));
}

#[tokio::test]
async fn manual_token_and_missing_token() {
    let http = client();
    let tokens = MemoryTokenStore::default();
    let ctx = AuthContext { tokens: &tokens, http: &http };
    let auth = Auth::new(AuthType::OAuth2)
        .with("grant_type", "authorization_code")
        .with("accessToken", "pasted")
        .with("headerPrefix", "Token");
    let mut r = req(auth, "r-1");
    merpati_auth::apply(&mut r, &ctx).await.unwrap();
    assert_eq!(r.header("authorization"), Some("Token pasted"));

    let mut r = req(Auth::new(AuthType::OAuth2).with("grant_type", "authorization_code"), "r-2");
    let applied = merpati_auth::apply(&mut r, &ctx).await.unwrap();
    assert!(r.header("authorization").is_none());
    assert!(applied.notes.iter().any(|n| n.contains("Get New Access Token")));

    // Stored token wins over the pasted one.
    tokens.set("r-3", Some(OAuth2Token { access_token: "stored".into(), ..Default::default() }));
    let mut r = req(Auth::new(AuthType::OAuth2).with("accessToken", "pasted"), "r-3");
    merpati_auth::apply(&mut r, &ctx).await.unwrap();
    assert_eq!(r.header("authorization"), Some("Bearer stored"));
}

#[tokio::test]
async fn authorization_code_with_pkce_via_browser_redirect() {
    let (server, _state) = start().await;
    let http = client();
    let port = free_port();
    let auth = base_auth(&server, "authorization_code_with_pkce")
        .with("redirect_uri", format!("http://localhost:{port}/callback"))
        .with("tokenName", "dev");
    let browser = client();
    // The "browser" follows the authorization server's redirect to our listener.
    let open = move |url: &str| {
        let b = browser.clone();
        let url = url.to_string();
        tokio::spawn(async move {
            let _ = b.get(url).send().await;
        });
    };
    let token = oauth2::get_new_token(&auth, &http, &open).await.unwrap();
    assert_eq!(token.access_token, "ac-token");
    assert_eq!(token.refresh_token.as_deref(), Some("r2"));
    assert_eq!(token.name, "dev");
    assert!(token.expires_at.is_some());
}

#[tokio::test]
async fn authorization_code_error_from_server() {
    let (server, _state) = start().await;
    let http = client();
    let port = free_port();
    let auth = base_auth(&server, "authorization_code")
        .with("clientId", "wrong")
        .with("redirect_uri", format!("http://127.0.0.1:{port}/cb"));
    let browser = client();
    let open = move |url: &str| {
        let b = browser.clone();
        let url = url.to_string();
        tokio::spawn(async move {
            let _ = b.get(url).send().await;
        });
    };
    let err = oauth2::get_new_token(&auth, &http, &open).await.unwrap_err();
    assert!(err.to_string().contains("unauthorized_client"), "{err}");
}

#[tokio::test]
async fn implicit_flow_reads_fragment_via_forwarding_page() {
    let http = client();
    let port = free_port();
    let auth = Auth::new(AuthType::OAuth2)
        .with("grant_type", "implicit")
        .with("authUrl", "http://idp.invalid/authorize")
        .with("clientId", "cid")
        .with("state", "s1")
        .with("redirect_uri", format!("http://localhost:{port}/cb"));
    let browser = client();
    // Simulate a browser: the IdP redirects to /cb#access_token=...; the browser
    // loads /cb (fragment not sent), runs the page script, then loads /cb?...
    let open = move |_url: &str| {
        let b = browser.clone();
        tokio::spawn(async move {
            let page = b.get(format!("http://127.0.0.1:{port}/cb")).send().await.unwrap().text().await.unwrap();
            assert!(page.contains("location.hash"));
            let _ = b
                .get(format!("http://127.0.0.1:{port}/cb?access_token=imp&token_type=bearer&expires_in=60&state=s1"))
                .send()
                .await;
        });
    };
    let token = oauth2::get_new_token(&auth, &http, &open).await.unwrap();
    assert_eq!(token.access_token, "imp");
}

#[tokio::test]
async fn get_new_token_for_direct_grants() {
    let (server, _) = start().await;
    let http = client();
    let never = |_: &str| panic!("no browser for client credentials");
    let t = oauth2::get_new_token(&base_auth(&server, "client_credentials"), &http, &never).await.unwrap();
    assert_eq!(t.access_token, "cc-token");
    let refreshed = oauth2::refresh(
        &base_auth(&server, "password_credentials"),
        &OAuth2Token { access_token: "old".into(), refresh_token: Some("r1".into()), ..Default::default() },
        &http,
    )
    .await
    .unwrap();
    assert_eq!(refreshed.access_token, "refreshed-token");
}
