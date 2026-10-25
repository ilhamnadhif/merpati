//! Manual testing: runs the capture proxy and prints entries as they finish.
//!
//! `cargo run -p merpati-proxy --example capture -- [port] [ca-dir]`
//!
//! The CA (cert and key) is kept in `ca-dir` (default: the system temp dir)
//! so the same CA survives restarts; trust `merpati-ca.pem` only for testing.

use std::sync::Arc;

use merpati_proxy::{CaPem, CaStore, CaptureProxy, CertificateAuthority, EntryKind, ProxyConfig, ProxyEvent};

struct DirStore(std::path::PathBuf);

impl CaStore for DirStore {
    fn load(&self) -> Result<Option<CaPem>, String> {
        let cert = self.0.join("merpati-ca.pem");
        let key = self.0.join("merpati-ca.key");
        if !cert.exists() || !key.exists() {
            return Ok(None);
        }
        let read = |p: &std::path::Path| std::fs::read_to_string(p).map_err(|e| e.to_string());
        Ok(Some(CaPem { cert_pem: read(&cert)?, key_pem: read(&key)? }))
    }

    fn save(&self, ca: &CaPem) -> Result<(), String> {
        std::fs::create_dir_all(&self.0).map_err(|e| e.to_string())?;
        std::fs::write(self.0.join("merpati-ca.pem"), &ca.cert_pem).map_err(|e| e.to_string())?;
        std::fs::write(self.0.join("merpati-ca.key"), &ca.key_pem).map_err(|e| e.to_string())
    }
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let port: u16 = args.next().and_then(|p| p.parse().ok()).unwrap_or(merpati_proxy::DEFAULT_PORT);
    let dir = args.next().map(Into::into).unwrap_or_else(|| std::env::temp_dir().join("merpati-proxy-example"));
    let store = DirStore(dir);
    let (ca, created) = CertificateAuthority::load_or_generate(&store).expect("CA");
    let ca_path = store.0.join("merpati-ca.pem");
    println!("CA {} ({}) at {}", ca.common_name(), if created { "new" } else { "loaded" }, ca_path.display());
    println!("trusted by the system: {}", ca.is_trusted_by_system());
    let config = ProxyConfig {
        bind: ([127, 0, 0, 1], port).into(),
        ca: Some(Arc::new(ca)),
        on_event: Some(Arc::new(|ev: &ProxyEvent| {
            if let ProxyEvent::Updated(e) = ev
                && e.is_finished()
            {
                let what = match e.kind {
                    EntryKind::Tunnel => {
                        let s = e.stream.clone().unwrap_or_default();
                        format!("tunnel up={} down={}", s.bytes_up, s.bytes_down)
                    }
                    EntryKind::WebSocket => format!("websocket {} messages", e.ws_messages.len()),
                    EntryKind::Http => format!(
                        "{} {}B {:?}",
                        e.status.map(|s| s.to_string()).unwrap_or_else(|| "---".into()),
                        e.response_body.size,
                        e.timings.total.unwrap_or_default()
                    ),
                };
                println!(
                    "#{} {} {} [{} -> {}] {}{}",
                    e.id,
                    e.method,
                    e.url,
                    e.http_version,
                    e.upstream_http_version.as_deref().unwrap_or("-"),
                    what,
                    e.error.as_ref().map(|err| format!(" ERROR {err}")).unwrap_or_default()
                );
            }
        })),
        ..ProxyConfig::default()
    };
    let proxy = CaptureProxy::start(config).await.expect("start proxy");
    println!(
        "proxy on {}  (try: curl -x {} --cacert {} https://example.com)",
        proxy.url(),
        proxy.url(),
        ca_path.display()
    );
    let _ = tokio::signal::ctrl_c().await;
    proxy.stop().await;
}
