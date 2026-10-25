//! Local mock server driven by collection examples (PRD FR-M1..M4).
//!
//! ```no_run
//! # async fn demo(collection: merpati_core::Collection) -> std::io::Result<()> {
//! let server = merpati_mock::MockServer::start(collection, merpati_mock::MockConfig::default()).await?;
//! println!("mock running at {}", server.url());
//! server.stop().await;
//! # Ok(())
//! # }
//! ```

pub mod listener;
mod matcher;
mod server;
mod template;

pub use listener::{ListenerConfig, ReplayResult, WebhookEvent, WebhookListener, replay as replay_webhook};
pub use matcher::{IncomingRequest, MatchResult, MatchScore, MockRoute, find_match, routes};
pub use server::{MockLogEntry, MockServer};
pub use template::TemplateContext;

use merpati_core::VariableScopes;

/// How a simulated failure manifests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    /// Answer with this status and a JSON error body.
    Status(u16),
    /// Abort the response so the client sees a broken connection.
    Drop,
    /// Never answer (until the client gives up, at most five minutes).
    Timeout,
}

impl std::str::FromStr for FailureKind {
    type Err = String;

    /// An HTTP status code (100-599), `drop` or `timeout`.
    fn from_str(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if s.eq_ignore_ascii_case("drop") {
            return Ok(FailureKind::Drop);
        }
        if s.eq_ignore_ascii_case("timeout") {
            return Ok(FailureKind::Timeout);
        }
        match s.parse::<u16>() {
            Ok(code) if (100..=599).contains(&code) => Ok(FailureKind::Status(code)),
            _ => Err(format!("expected an HTTP status code (100-599), `drop` or `timeout`, got \"{s}\"")),
        }
    }
}

/// Randomly fail a fraction of requests (FR-M2 "simulasi kegagalan").
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FailureSimulation {
    /// Probability in `0.0..=1.0` that a request fails.
    pub rate: f32,
    pub kind: FailureKind,
}

impl FailureSimulation {
    pub fn status(rate: f32, status: u16) -> Self {
        FailureSimulation { rate, kind: FailureKind::Status(status) }
    }

    pub fn drop_connection(rate: f32) -> Self {
        FailureSimulation { rate, kind: FailureKind::Drop }
    }

    pub fn timeout(rate: f32) -> Self {
        FailureSimulation { rate, kind: FailureKind::Timeout }
    }
}

/// Network conditions applied to every mock response (FR-M2 "simulasi
/// kondisi jaringan"): latency with jitter and limited bandwidth.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NetworkConditions {
    /// Fixed latency before the response starts.
    pub latency_ms: u64,
    /// Random extra latency, uniformly `0..=jitter_ms`.
    pub jitter_ms: u64,
    /// Response bodies are sent at most this fast; `None` = unlimited.
    pub bandwidth_bytes_per_sec: Option<u64>,
}

impl NetworkConditions {
    /// Named presets, roughly like browser dev tools' throttling profiles.
    pub const PRESETS: [(&'static str, NetworkConditions); 4] = [
        ("Fast 4G", NetworkConditions { latency_ms: 40, jitter_ms: 20, bandwidth_bytes_per_sec: Some(1_500_000) }),
        ("Fast 3G", NetworkConditions { latency_ms: 150, jitter_ms: 50, bandwidth_bytes_per_sec: Some(180_000) }),
        ("Slow 3G", NetworkConditions { latency_ms: 400, jitter_ms: 150, bandwidth_bytes_per_sec: Some(50_000) }),
        ("Satellite", NetworkConditions { latency_ms: 600, jitter_ms: 100, bandwidth_bytes_per_sec: Some(250_000) }),
    ];
}

#[derive(Clone, Debug)]
pub struct MockConfig {
    /// Interface to bind, default `127.0.0.1`.
    pub host: String,
    /// 0 picks a free port.
    pub port: u16,
    /// Artificial latency added to every response.
    pub delay_ms: u64,
    /// Latency jitter and bandwidth, on top of `delay_ms`.
    pub network: NetworkConditions,
    /// Resolves `{{vars}}` in example URLs and response templates.
    pub variables: VariableScopes,
    /// Answer CORS preflights and add permissive CORS headers.
    pub cors: bool,
    /// Number of requests kept in the log ring buffer.
    pub log_capacity: usize,
    pub failure: Option<FailureSimulation>,
}

impl Default for MockConfig {
    fn default() -> Self {
        MockConfig {
            host: "127.0.0.1".into(),
            port: 0,
            delay_ms: 0,
            network: NetworkConditions::default(),
            variables: VariableScopes::default(),
            cors: true,
            log_capacity: 500,
            failure: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_kinds_parse() {
        assert_eq!("503".parse(), Ok(FailureKind::Status(503)));
        assert_eq!(" Drop ".parse(), Ok(FailureKind::Drop));
        assert_eq!("timeout".parse(), Ok(FailureKind::Timeout));
        for bad in ["", "abc", "5xx", "99", "600", "70000", "-1"] {
            assert!(bad.parse::<FailureKind>().is_err(), "{bad}");
        }
    }
}
