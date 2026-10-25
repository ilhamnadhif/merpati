//! Provider tests against mock servers, plus cache and lookup tests.
//!
//! Mock servers run on their own thread and runtime, so tests may block the
//! test thread (the synchronous lookup) without starving the server.

mod aws;
mod azure;
mod cache;
mod onepassword;
mod vault;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use merpati_core::secrets::SecretStore;

use crate::{ExternalSecrets, ProviderConfig, ProviderSettings, SecretsConfig};

/// A request as seen by a mock.
pub(crate) struct Req {
    pub method: Method,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl Req {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    pub fn param(&self, name: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    pub fn form(&self) -> Vec<(String, String)> {
        url::form_urlencoded::parse(&self.body).map(|(k, v)| (k.into_owned(), v.into_owned())).collect()
    }

    pub fn bearer(&self) -> Option<&str> {
        self.header("authorization").and_then(|h| h.strip_prefix("Bearer "))
    }
}

pub(crate) fn json(status: u16, v: Value) -> Response {
    (StatusCode::from_u16(status).unwrap(), axum::Json(v)).into_response()
}

pub(crate) struct MockServer {
    pub url: String,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for MockServer {
    fn drop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
    }
}

/// Serves `handler` for every request on a fresh port.
pub(crate) fn serve<F, Fut>(handler: F) -> MockServer
where
    F: Fn(Req) -> Fut + Clone + Send + Sync + 'static,
    Fut: std::future::Future<Output = Response> + Send + 'static,
{
    let router =
        Router::new().fallback(move |method: Method, uri: Uri, headers: HeaderMap, body: axum::body::Bytes| {
            let handler = handler.clone();
            async move {
                let query = uri
                    .query()
                    .map(|q| {
                        url::form_urlencoded::parse(q.as_bytes())
                            .map(|(k, v)| (k.into_owned(), v.into_owned()))
                            .collect()
                    })
                    .unwrap_or_default();
                let path = percent_encoding::percent_decode_str(uri.path()).decode_utf8_lossy().to_string();
                handler(Req { method, path, query, headers, body: body.to_vec() }).await
            }
        });
    let (addr_tx, addr_rx) = std::sync::mpsc::channel::<SocketAddr>();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            addr_tx.send(listener.local_addr().unwrap()).unwrap();
            let server = axum::serve(listener, router);
            tokio::select! {
                _ = server => {}
                _ = stop_rx => {}
            }
        });
    });
    let addr = addr_rx.recv().unwrap();
    MockServer { url: format!("http://{addr}"), stop: Some(stop_tx) }
}

/// A client that never uses a proxy (tests talk to 127.0.0.1).
pub(crate) fn client() -> reqwest::Client {
    merpati_http::tls::install_provider();
    reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).build().unwrap()
}

/// A manager with one provider `p1` and an in-memory secret store.
pub(crate) fn manager(settings: ProviderSettings) -> ExternalSecrets {
    let store = Arc::new(SecretStore::memory());
    let config = SecretsConfig {
        providers: vec![ProviderConfig { id: "p1".into(), name: "Test".into(), settings }],
        links: Vec::new(),
    };
    ExternalSecrets::new(store, config, client())
}

/// Writes an executable shell script.
#[cfg(unix)]
pub(crate) fn script(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    let tmp = dir.join(format!(".{name}.tmp"));
    {
        let mut f = std::fs::File::create(&tmp).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        f.sync_all().unwrap();
    }
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::rename(&tmp, &path).unwrap();
    path
}
