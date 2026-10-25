//! Local capture proxy for Merpati (like Postman's capture proxy).
//!
//! The user points their own browser, app or device at the proxy; every
//! request that goes through is recorded and can be saved into a collection.
//!
//! * Plain HTTP: absolute-form requests are forwarded (keep-alive, chunked,
//!   bodies streamed; only the first [`ProxyConfig::max_body_bytes`] of each
//!   body are kept in memory; `OPTIONS http://host` becomes `OPTIONS *`).
//! * HTTPS: `CONNECT` is either decrypted with a local CA
//!   ([`CertificateAuthority`], leaf certificates generated per host from the
//!   client's SNI) or tunnelled as opaque bytes (interception off,
//!   passthrough/excluded hosts, non-HTTP protocols). Upstream certificates
//!   are verified unless [`ProxyConfig::verify_upstream_tls`] is turned off.
//! * HTTP/2 on both sides (negotiated with ALPN), WebSocket upgrades relayed
//!   with message capture, gRPC trailers preserved.
//! * Optional chaining through an upstream HTTP proxy.
//!
//! ```no_run
//! # async fn demo() -> Result<(), merpati_proxy::ProxyError> {
//! use std::sync::Arc;
//! use merpati_proxy::{CaptureProxy, CertificateAuthority, MemoryCaStore, ProxyConfig, ImportOptions};
//!
//! let (ca, _created) = CertificateAuthority::load_or_generate(&MemoryCaStore::new())?;
//! let proxy = CaptureProxy::start(ProxyConfig { ca: Some(Arc::new(ca)), ..ProxyConfig::default() }).await?;
//! println!("set HTTP(S) proxy to {}", proxy.url());
//! // ... later
//! for entry in proxy.entries() {
//!     if let Some(item) = entry.to_request_item(&ImportOptions::default()) {
//!         println!("{}", item.name);
//!     }
//! }
//! proxy.stop().await;
//! # Ok(()) }
//! ```
//!
//! # Trusting the CA
//!
//! Merpati never modifies the system trust store. The user installs
//! [`CertificateAuthority::cert_pem`] (or [`cert_der`](CertificateAuthority::cert_der))
//! themselves; [`CertificateAuthority::trust_instructions`] returns this text
//! for the UI. Devices can download it from `http://merpati.proxy/` once
//! their proxy points at Merpati.
//!
//! macOS, current user (asks for the login password):
//!
//! ```text
//! security add-trusted-cert -r trustRoot -k ~/Library/Keychains/login.keychain-db merpati-ca.pem
//! ```
//!
//! macOS, all users:
//!
//! ```text
//! sudo security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain merpati-ca.pem
//! ```
//!
//! Remove it again with `security delete-certificate -c "<CA common name>" ~/Library/Keychains/login.keychain-db`
//! (or in Keychain Access). Firefox has its own store (Settings > Certificates > Import, or
//! `security.enterprise_roots.enabled`); curl takes `--cacert`, Node.js `NODE_EXTRA_CA_CERTS`.
//!
//! # Security notes
//!
//! * The proxy listens on loopback unless [`ProxyConfig::allow_lan`] is set;
//!   on the LAN anyone who can reach the port can use it (and reach this
//!   machine's local services through it).
//! * The CA private key only lives in memory and wherever the host persists
//!   it through [`CaStore`]; keep it in the keychain.
//! * Captured headers and bodies (cookies, tokens) stay in the capture store;
//!   the proxy logs only host names and errors.

mod ca;
mod config;
mod convert;
mod entry;
mod error;
mod exchange;
mod har;
mod info;
mod io;
mod server;
mod store;
mod upstream;
mod ws;

pub use ca::{CaPem, CaStore, CertificateAuthority, MemoryCaStore, trust_instructions};
pub use config::{CaptureFilter, DEFAULT_PORT, EventCallback, ProxyConfig, UpstreamProxy, host_matches};
pub use convert::{ImportOptions, to_request_items};
pub use entry::{
    CaptureEntry, CapturedBody, DEFAULT_DECODE_LIMIT, DecodedBody, EntryKind, EntryState, StreamStats, Timings,
    WsMessage, WsOpcode, decode_body,
};
pub use error::ProxyError;
pub use har::to_har;
pub use server::{CaptureProxy, INFO_HOST, ProxyStats};
pub use store::ProxyEvent;
