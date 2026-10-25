//! The MCP authorization flow against an axum protected resource plus a fake
//! authorization server: protected resource metadata, AS discovery (RFC 8414,
//! OpenID Connect, 2025-03-26 defaults), dynamic client registration, the
//! authorize redirect (driven by the test instead of a browser), PKCE, the
//! token and refresh grants and the retry after a 401.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::{Form, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Json, Redirect, Response};
use axum::routing::{get, post};
use base64::Engine;
use merpati_core::protocols::{McpRequest, McpTransport};
use merpati_mcp::{
    CancellationToken, McpClient, McpConfig, McpError, McpEvent, MemoryOAuthStorage, OAuthOptions, OAuthSettings,
    OAuthStorage,
};
use parking_lot::Mutex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Default, PartialEq)]
enum Mode {
    /// PRM in WWW-Authenticate, AS metadata at the RFC 8414 path-aware URL.
    #[default]
    Standard,
    /// No `resource_metadata` in the header; OIDC discovery appended to the issuer path.
    WellKnownOidc,
    /// 2025-03-26: no PRM and no metadata; default endpoints at the origin.
    Legacy,
}

struct Grant {
    client_id: String,
    redirect_uri: String,
    challenge: String,
    resource: String,
    scope: String,
}

#[derive(Default)]
struct Fake {
    base: Mutex<String>,
    mode: Mode,
    deny: bool,
    /// Advertise a non-http authorization endpoint.
    evil_endpoint: bool,
    /// Pre-registered confidential client (id, secret).
    fixed_client: Option<(String, String)>,
    registrations: Mutex<Vec<Value>>,
    clients: Mutex<HashMap<String, Vec<String>>>,
    codes: Mutex<HashMap<String, Grant>>,
    /// Valid access token -> resource.
    access: Mutex<HashMap<String, String>>,
    refresh: Mutex<HashSet<String>>,
    authorize_params: Mutex<Vec<HashMap<String, String>>>,
    token_forms: Mutex<Vec<HashMap<String, String>>>,
    basic_auth: Mutex<Vec<String>>,
    unauthorized: AtomicUsize,
    refreshes: AtomicUsize,
    next: AtomicUsize,
}

impl Fake {
    fn base(&self) -> String {
        self.base.lock().clone()
    }

    fn issuer(&self) -> String {
        match self.mode {
            Mode::Legacy => self.base(),
            _ => format!("{}/auth", self.base()),
        }
    }

    fn as_metadata(&self) -> Value {
        let i = self.issuer();
        let authorize = if self.evil_endpoint { "file:///etc/passwd".to_string() } else { format!("{i}/authorize") };
        json!({
            "issuer": i,
            "authorization_endpoint": authorize,
            "token_endpoint": format!("{i}/token"),
            "registration_endpoint": format!("{i}/register"),
            "response_types_supported": ["code"],
            "grant_types_supported": ["authorization_code", "refresh_token"],
            "code_challenge_methods_supported": ["S256"],
            "token_endpoint_auth_methods_supported": ["none", "client_secret_basic"]
        })
    }

    fn issue(&self, resource: &str) -> Value {
        let n = self.next.fetch_add(1, Ordering::SeqCst) + 1;
        let (at, rt) = (format!("at-{n}"), format!("rt-{n}"));
        self.access.lock().insert(at.clone(), resource.to_string());
        self.refresh.lock().insert(rt.clone());
        json!({"access_token": at, "token_type": "Bearer", "expires_in": 3600, "refresh_token": rt, "scope": "mcp:tools"})
    }
}

type St = State<Arc<Fake>>;

fn rpc(id: &Value, result: Value) -> Response {
    ([("mcp-session-id", "oauth-sess")], Json(json!({"jsonrpc": "2.0", "id": id, "result": result}))).into_response()
}

async fn mcp(State(f): St, headers: HeaderMap, body: String) -> Response {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
        .to_string();
    let expected = format!("{}/mcp", f.base());
    if f.access.lock().get(&token) != Some(&expected) {
        f.unauthorized.fetch_add(1, Ordering::SeqCst);
        let www = match f.mode {
            Mode::Standard => format!(
                "Bearer error=\"invalid_token\", resource_metadata=\"{}/.well-known/oauth-protected-resource/mcp\"",
                f.base()
            ),
            _ => "Bearer realm=\"mcp\"".to_string(),
        };
        return (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, www)], Json(json!({"error": "invalid_token"})))
            .into_response();
    }
    let msg: Value = serde_json::from_str(&body).unwrap();
    let Some(id) = msg.get("id").cloned() else { return StatusCode::ACCEPTED.into_response() };
    match msg["method"].as_str().unwrap_or("") {
        "initialize" => rpc(
            &id,
            json!({"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "protected", "version": "1"}}),
        ),
        "tools/call" => rpc(&id, json!({"content": [{"type": "text", "text": token}]})),
        _ => rpc(&id, json!({})),
    }
}

async fn prm(State(f): St) -> Response {
    if f.mode == Mode::Legacy {
        return StatusCode::NOT_FOUND.into_response();
    }
    Json(json!({
        "resource": format!("{}/mcp", f.base()),
        "authorization_servers": [f.issuer()],
        "scopes_supported": ["mcp:tools"],
        "bearer_methods_supported": ["header"]
    }))
    .into_response()
}

async fn rfc8414(State(f): St) -> Response {
    if f.mode != Mode::Standard {
        return StatusCode::NOT_FOUND.into_response();
    }
    Json(f.as_metadata()).into_response()
}

async fn oidc(State(f): St) -> Response {
    if f.mode != Mode::WellKnownOidc {
        return StatusCode::NOT_FOUND.into_response();
    }
    Json(f.as_metadata()).into_response()
}

async fn register(State(f): St, Json(body): Json<Value>) -> Response {
    f.registrations.lock().push(body.clone());
    let n = f.next.fetch_add(1, Ordering::SeqCst);
    let id = format!("dyn-client-{n}");
    let uris: Vec<String> =
        body["redirect_uris"].as_array().unwrap().iter().map(|u| u.as_str().unwrap().to_string()).collect();
    f.clients.lock().insert(id.clone(), uris.clone());
    (
        StatusCode::CREATED,
        Json(json!({"client_id": id, "redirect_uris": uris, "token_endpoint_auth_method": "none", "client_id_issued_at": 1})),
    )
        .into_response()
}

async fn authorize(State(f): St, Query(q): Query<HashMap<String, String>>) -> Response {
    f.authorize_params.lock().push(q.clone());
    let get = |k: &str| q.get(k).cloned().unwrap_or_default();
    let client_id = get("client_id");
    let redirect_uri = get("redirect_uri");
    let known = match &f.fixed_client {
        Some((id, _)) if *id == client_id => true,
        _ => f.clients.lock().get(&client_id).is_some_and(|u| u.contains(&redirect_uri)),
    };
    if !known || get("response_type") != "code" || get("code_challenge_method") != "S256" || get("state").is_empty() {
        return (StatusCode::BAD_REQUEST, "bad authorization request").into_response();
    }
    let sep = if redirect_uri.contains('?') { '&' } else { '?' };
    if f.deny {
        return Redirect::to(&format!("{redirect_uri}{sep}error=access_denied&state={}", get("state"))).into_response();
    }
    let code = format!("code-{}", f.next.fetch_add(1, Ordering::SeqCst));
    f.codes.lock().insert(
        code.clone(),
        Grant {
            client_id,
            redirect_uri: redirect_uri.clone(),
            challenge: get("code_challenge"),
            resource: get("resource"),
            scope: get("scope"),
        },
    );
    Redirect::to(&format!("{redirect_uri}{sep}code={code}&state={}", get("state"))).into_response()
}

fn oauth_error(error: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"error": error}))).into_response()
}

async fn token(State(f): St, headers: HeaderMap, Form(form): Form<HashMap<String, String>>) -> Response {
    f.token_forms.lock().push(form.clone());
    let basic = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
        .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
        .map(|b| String::from_utf8(b).unwrap());
    if let Some(b) = &basic {
        f.basic_auth.lock().push(b.clone());
    }
    let client_id = match (&basic, form.get("client_id")) {
        (Some(b), _) => b.split(':').next().unwrap().to_string(),
        (None, Some(c)) => c.clone(),
        _ => return oauth_error("invalid_client"),
    };
    if let Some((id, secret)) = &f.fixed_client
        && *id == client_id
        && basic.as_deref() != Some(&format!("{id}:{secret}"))
    {
        return oauth_error("invalid_client");
    }
    let get = |k: &str| form.get(k).cloned().unwrap_or_default();
    match get("grant_type").as_str() {
        "authorization_code" => {
            let Some(g) = f.codes.lock().remove(&get("code")) else { return oauth_error("invalid_grant") };
            let verifier_ok = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(Sha256::digest(get("code_verifier").as_bytes()))
                == g.challenge;
            if !verifier_ok || g.client_id != client_id || g.redirect_uri != get("redirect_uri") {
                return oauth_error("invalid_grant");
            }
            if get("resource") != g.resource {
                return oauth_error("invalid_target");
            }
            let _ = g.scope;
            Json(f.issue(&g.resource)).into_response()
        }
        "refresh_token" => {
            if !f.refresh.lock().remove(&get("refresh_token")) {
                return oauth_error("invalid_grant");
            }
            f.refreshes.fetch_add(1, Ordering::SeqCst);
            Json(f.issue(&get("resource"))).into_response()
        }
        _ => oauth_error("unsupported_grant_type"),
    }
}

async fn spawn(fake: Fake) -> Arc<Fake> {
    merpati_http::tls::install_provider();
    let fake = Arc::new(fake);
    let app = Router::new()
        .route("/mcp", post(mcp))
        .route("/.well-known/oauth-protected-resource/mcp", get(prm))
        .route("/.well-known/oauth-authorization-server/auth", get(rfc8414))
        .route("/auth/.well-known/openid-configuration", get(oidc))
        .route("/auth/register", post(register))
        .route("/auth/authorize", get(authorize))
        .route("/auth/token", post(token))
        .route("/register", post(register))
        .route("/authorize", get(authorize))
        .route("/token", post(token))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    *fake.base.lock() = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    fake
}

fn config(fake: &Fake) -> McpConfig {
    let req = McpRequest {
        transport: McpTransport::Http { url: format!("{}/mcp", fake.base()), headers: vec![] },
        ..Default::default()
    };
    let mut cfg = McpConfig::from_request(&req, &|s| s.to_string());
    cfg.timeout = Duration::from_secs(10);
    cfg
}

/// Options whose "browser" follows the authorization URL with reqwest (the
/// fake AS redirects straight to the loopback callback).
fn browser(storage: Arc<dyn OAuthStorage>, opened: Arc<AtomicUsize>) -> OAuthOptions {
    OAuthOptions::new(move |url: &str| {
        opened.fetch_add(1, Ordering::SeqCst);
        let url = url.to_string();
        tokio::spawn(async move {
            let _ = reqwest::Client::new().get(url).send().await;
        });
    })
    .with_storage(storage)
    .with_login_timeout(Duration::from_secs(10))
}

type Events = Arc<Mutex<Vec<McpEvent>>>;

fn recorder() -> (Events, impl Fn(McpEvent) + Send + Sync + 'static) {
    let events: Events = Arc::default();
    let sink = events.clone();
    (events, move |e| sink.lock().push(e))
}

#[tokio::test]
async fn full_flow_refresh_and_reuse() {
    let fake = spawn(Fake::default()).await;
    let storage = Arc::new(MemoryOAuthStorage::default());
    let opened = Arc::new(AtomicUsize::new(0));
    let (events, on_event) = recorder();
    let client = McpClient::builder(config(&fake))
        .oauth(browser(storage.clone(), opened.clone()))
        .on_event(on_event)
        .connect()
        .await
        .expect("connect after authorization");
    assert_eq!(client.server_info().name, "protected");
    assert_eq!(opened.load(Ordering::SeqCst), 1);

    // Dynamic client registration with the loopback redirect.
    let regs = fake.registrations.lock().clone();
    assert_eq!(regs.len(), 1);
    let redirect = regs[0]["redirect_uris"][0].as_str().unwrap().to_string();
    assert!(redirect.starts_with("http://127.0.0.1:") && redirect.ends_with("/callback"), "{redirect}");
    assert_eq!(regs[0]["token_endpoint_auth_method"], "none");
    assert_eq!(regs[0]["grant_types"], json!(["authorization_code", "refresh_token"]));
    assert_eq!(regs[0]["scope"], "mcp:tools");

    // Authorization request: PKCE S256, RFC 8707 resource, PRM scope.
    let base = fake.base();
    let auth = fake.authorize_params.lock()[0].clone();
    assert_eq!(auth["resource"], format!("{base}/mcp"));
    assert_eq!(auth["scope"], "mcp:tools");
    assert_eq!(auth["code_challenge_method"], "S256");
    let form = fake.token_forms.lock()[0].clone();
    assert_eq!(form["resource"], format!("{base}/mcp"));
    assert_eq!(form["client_id"], "dyn-client-0");
    assert!(form["code_verifier"].len() >= 43);

    // Persisted.
    let key = merpati_mcp::oauth_server_key(&format!("{base}/mcp")).unwrap();
    let stored = storage.load_tokens(&key).expect("tokens stored");
    assert_eq!(stored.issuer, format!("{base}/auth"));
    assert_eq!(stored.client_id, "dyn-client-0");
    assert!(stored.refresh_token.is_some() && stored.expires_at.is_some());
    assert_eq!(storage.load_client(&format!("{base}/auth")).unwrap().client_id, "dyn-client-0");
    let first = stored.access_token.clone();
    assert_eq!(client.call_tool("whoami", json!({})).await.unwrap().text(), first);

    // The server revokes the token: 401, refresh, retry.
    fake.access.lock().clear();
    let after = client.call_tool("whoami", json!({})).await.unwrap().text();
    assert_ne!(after, first);
    assert_eq!(fake.refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(opened.load(Ordering::SeqCst), 1, "no second browser login");
    assert_eq!(storage.load_tokens(&key).unwrap().access_token, after);
    assert!(fake.token_forms.lock().last().unwrap().get("resource").is_some(), "refresh sends resource");
    client.close().await;

    // A new connection reuses the stored token.
    let unauthorized = fake.unauthorized.load(Ordering::SeqCst);
    let client =
        McpClient::builder(config(&fake)).oauth(browser(storage.clone(), opened.clone())).connect().await.unwrap();
    assert_eq!(client.call_tool("whoami", json!({})).await.unwrap().text(), after);
    assert_eq!(fake.unauthorized.load(Ordering::SeqCst), unauthorized, "no 401 with a stored token");
    client.close().await;

    // An expired stored token is refreshed before the first request.
    let mut t = storage.load_tokens(&key).unwrap();
    t.expires_at = Some(1);
    storage.save_tokens(&key, Some(&t));
    let client =
        McpClient::builder(config(&fake)).oauth(browser(storage.clone(), opened.clone())).connect().await.unwrap();
    assert_eq!(fake.refreshes.load(Ordering::SeqCst), 2);
    assert_eq!(fake.unauthorized.load(Ordering::SeqCst), unauthorized);
    assert_eq!(opened.load(Ordering::SeqCst), 1);
    client.close().await;

    // The refresh token is dead too: the stored registration is reused for a new login.
    fake.access.lock().clear();
    fake.refresh.lock().clear();
    let client =
        McpClient::builder(config(&fake)).oauth(browser(storage.clone(), opened.clone())).connect().await.unwrap();
    assert_eq!(opened.load(Ordering::SeqCst), 2);
    assert_eq!(fake.registrations.lock().len(), 1, "registered client reused (same redirect port)");
    client.close().await;

    // Nothing secret in the log (Traffic carries the fake tool's echo of the token).
    let log: Vec<String> = events
        .lock()
        .iter()
        .filter_map(|e| match e {
            McpEvent::Log { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect();
    let log = log.join("\n");
    assert!(!log.contains(&first) && !log.contains("rt-"), "{log}");
}

#[tokio::test]
async fn well_known_fallback_with_oidc_discovery() {
    let fake = spawn(Fake { mode: Mode::WellKnownOidc, ..Default::default() }).await;
    let opened = Arc::new(AtomicUsize::new(0));
    let client = McpClient::builder(config(&fake))
        .oauth(browser(Arc::new(MemoryOAuthStorage::default()), opened.clone()))
        .connect()
        .await
        .expect("connect");
    assert_eq!(opened.load(Ordering::SeqCst), 1);
    assert_eq!(fake.authorize_params.lock()[0]["resource"], format!("{}/mcp", fake.base()));
    client.close().await;
}

#[tokio::test]
async fn spec_2025_03_26_default_endpoints() {
    let fake = spawn(Fake { mode: Mode::Legacy, ..Default::default() }).await;
    let opened = Arc::new(AtomicUsize::new(0));
    let storage = Arc::new(MemoryOAuthStorage::default());
    let client = McpClient::builder(config(&fake))
        .oauth(browser(storage.clone(), opened.clone()))
        .connect()
        .await
        .expect("connect");
    assert_eq!(fake.registrations.lock().len(), 1, "registered at /register");
    let auth = fake.authorize_params.lock()[0].clone();
    assert_eq!(auth["resource"], format!("{}/mcp", fake.base()));
    assert!(!auth.contains_key("scope"), "no scope known");
    assert!(storage.load_client(&fake.base()).is_some(), "keyed by the origin as issuer");
    client.close().await;
}

#[tokio::test]
async fn preregistered_confidential_client() {
    let fake = spawn(Fake { fixed_client: Some(("fixed".into(), "s3cret".into())), ..Default::default() }).await;
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let mut cfg = config(&fake);
    cfg.oauth = Some(OAuthSettings {
        client_id: "fixed".into(),
        client_secret: "s3cret".into(),
        scope: "custom".into(),
        redirect_uri: format!("http://localhost:{port}/cb"),
        ..Default::default()
    });
    let opened = Arc::new(AtomicUsize::new(0));
    let client = McpClient::builder(cfg)
        .oauth(browser(Arc::new(MemoryOAuthStorage::default()), opened))
        .connect()
        .await
        .expect("connect");
    assert!(fake.registrations.lock().is_empty(), "no dynamic registration");
    let auth = fake.authorize_params.lock()[0].clone();
    assert_eq!((auth["client_id"].as_str(), auth["scope"].as_str()), ("fixed", "custom"));
    assert_eq!(auth["redirect_uri"], format!("http://localhost:{port}/cb"));
    assert_eq!(fake.basic_auth.lock().as_slice(), ["fixed:s3cret"]);
    assert!(!fake.token_forms.lock()[0].contains_key("client_secret"));
    client.close().await;
}

#[tokio::test]
async fn cancelling_the_login() {
    let fake = spawn(Fake::default()).await;
    let cancel = CancellationToken::new();
    let opts = OAuthOptions::new(|_url: &str| {}).with_cancel(cancel.clone());
    let mut cfg = config(&fake);
    // Shorter than the wait: the request timeout is paused during the login.
    cfg.timeout = Duration::from_millis(200);
    let c = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(800)).await;
        c.cancel();
    });
    let (events, on_event) = recorder();
    let err = McpClient::builder(cfg).oauth(opts).on_event(on_event).connect().await.unwrap_err();
    assert!(matches!(err, McpError::Cancelled(_)), "{err:?}");
    assert!(events.lock().iter().any(|e| matches!(e, McpEvent::Log { level, .. } if level == "auth")));
}

#[tokio::test]
async fn denied_login_is_reported() {
    let fake = spawn(Fake { deny: true, ..Default::default() }).await;
    let opened = Arc::new(AtomicUsize::new(0));
    let err = McpClient::builder(config(&fake))
        .oauth(browser(Arc::new(MemoryOAuthStorage::default()), opened))
        .connect()
        .await
        .unwrap_err();
    assert!(matches!(&err, McpError::Cancelled(m) if m.contains("access_denied")), "{err:?}");
}

#[tokio::test]
async fn static_bearer_header_skips_oauth() {
    let fake = spawn(Fake::default()).await;
    let req = McpRequest {
        transport: McpTransport::Http { url: format!("{}/mcp", fake.base()), headers: vec![] },
        auth: merpati_core::auth::Auth::bearer("stale"),
        ..Default::default()
    };
    let cfg = McpConfig::from_request(&req, &|s| s.to_string());
    let opened = Arc::new(AtomicUsize::new(0));
    let err = McpClient::builder(cfg)
        .oauth(browser(Arc::new(MemoryOAuthStorage::default()), opened.clone()))
        .connect()
        .await
        .unwrap_err();
    assert!(matches!(err, McpError::Http { status: 401, .. }), "{err:?}");
    assert_eq!(opened.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn non_http_authorization_endpoint_is_refused() {
    let fake = spawn(Fake { evil_endpoint: true, ..Default::default() }).await;
    let opened = Arc::new(AtomicUsize::new(0));
    let err = McpClient::builder(config(&fake))
        .oauth(browser(Arc::new(MemoryOAuthStorage::default()), opened.clone()))
        .connect()
        .await
        .unwrap_err();
    assert!(matches!(&err, McpError::Auth(m) if m.contains("refusing")), "{err:?}");
    assert_eq!(opened.load(Ordering::SeqCst), 0, "nothing handed to the browser");
}

#[tokio::test]
async fn unauthorized_without_any_metadata_suggests_a_static_credential() {
    merpati_http::tls::install_provider();
    let app = Router::new().route("/mcp", post(|| async { StatusCode::UNAUTHORIZED }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let req = McpRequest {
        transport: McpTransport::Http { url: format!("http://{addr}/mcp"), headers: vec![] },
        ..Default::default()
    };
    let opened = Arc::new(AtomicUsize::new(0));
    let err = McpClient::builder(McpConfig::from_request(&req, &|s| s.to_string()))
        .oauth(browser(Arc::new(MemoryOAuthStorage::default()), opened.clone()))
        .connect()
        .await
        .unwrap_err();
    assert!(matches!(&err, McpError::Auth(m) if m.contains("static credential")), "{err:?}");
    assert_eq!(opened.load(Ordering::SeqCst), 0);
}

type Pages = Arc<Mutex<Vec<(u16, String)>>>;

/// A "browser" that first sends stray requests to the callback (another
/// login's state, no state, nothing), then finishes the real login; or,
/// with `bogus_code`, comes back with the right state but a code the
/// authorization server never issued. Records every page it got.
fn stray_browser(pages: Pages, bogus_code: bool) -> OAuthOptions {
    OAuthOptions::new(move |url: &str| {
        let (url, pages) = (url.to_string(), pages.clone());
        tokio::spawn(async move {
            let q: HashMap<String, String> = reqwest::Url::parse(&url).unwrap().query_pairs().into_owned().collect();
            let cb = q["redirect_uri"].clone();
            let http = reqwest::Client::new();
            let mut urls = vec![
                format!("{cb}?code=forged&state=wrong"),
                format!("{cb}?code=forged"),
                format!("{cb}?error=access_denied&state=wrong"),
                cb.clone(),
            ];
            urls.push(if bogus_code { format!("{cb}?code=bogus&state={}", q["state"]) } else { url });
            for u in urls {
                let r = http.get(u).send().await.unwrap();
                let status = r.status().as_u16();
                let body = r.text().await.unwrap();
                pages.lock().push((status, body));
            }
        });
    })
    .with_storage(Arc::new(MemoryOAuthStorage::default()))
    .with_login_timeout(Duration::from_secs(10))
}

#[tokio::test]
async fn stray_callback_requests_do_not_abort_the_login() {
    let fake = spawn(Fake::default()).await;
    let pages: Pages = Arc::default();
    let client = McpClient::builder(config(&fake))
        .oauth(stray_browser(pages.clone(), false))
        .connect()
        .await
        .expect("the real redirect still completes the login");
    let pages = pages.lock().clone();
    assert_eq!(pages.len(), 5, "{pages:#?}");
    for (status, body) in &pages[..3] {
        assert_eq!(*status, 400);
        assert!(body.contains("does not belong to the login"), "{body}");
        assert!(!body.contains("Authorization complete"), "{body}");
    }
    assert!(pages[3].0 == 400 && pages[3].1.contains("Missing authorization code"), "{:?}", pages[3]);
    assert_eq!(pages[4].0, 200);
    assert!(pages[4].1.contains("Authorization complete"), "{}", pages[4].1);
    client.close().await;
}

#[tokio::test]
async fn the_callback_page_reports_a_failed_code_exchange() {
    let fake = spawn(Fake::default()).await;
    let pages: Pages = Arc::default();
    let err = McpClient::builder(config(&fake)).oauth(stray_browser(pages.clone(), true)).connect().await.unwrap_err();
    assert!(matches!(&err, McpError::Auth(m) if m.contains("token request failed")), "{err:?}");
    // The browser is told so instead of "complete".
    for _ in 0..100 {
        if pages.lock().len() == 5 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (status, body) = pages.lock().last().cloned().unwrap();
    assert_eq!(status, 200);
    assert!(body.contains("Authorization failed") && body.contains("token request failed"), "{body}");
    assert!(!body.contains("Authorization complete"), "{body}");
}
