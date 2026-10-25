//! Protocol clients for Merpati: the HTTP engine (redirects, cookies, proxy,
//! certificates, streaming of large bodies to disk), WebSocket sessions, the
//! client-certificate loading shared by every TLS client ([`certs`]) and the
//! proxy selection and tunnels shared by every other client ([`proxy`]),
//! plus the HTTP authentication primitives they share with `merpati-auth`
//! ([`challenge`], [`digest`]).

pub mod certs;
pub mod challenge;
pub mod cookies;
pub mod digest;
pub mod engine;
pub mod error;
pub mod proxy;
pub mod tls;
pub mod ws;

pub use certs::{ClientIdentity, SecretResolver};
pub use cookies::{CookieInfo, CookieJar};
pub use engine::{EngineConfig, HttpEngine, Progress, SendOptions, SendOutcome, SentRequest};
pub use error::{HttpError, HttpErrorKind};
pub use tokio_util::sync::CancellationToken;
