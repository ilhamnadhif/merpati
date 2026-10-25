//! Errors reported by the MCP client.

use std::time::Duration;

use serde_json::Value;

#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum McpError {
    /// The configuration cannot be used (empty command, invalid URL or header).
    #[error("{0}")]
    Config(String),
    /// The server process could not be started.
    #[error("{0}")]
    Spawn(String),
    /// Network or I/O failure while talking to the server.
    #[error("{0}")]
    Transport(String),
    /// The HTTP endpoint answered with a non-success status.
    #[error("HTTP {status}{}", if message.is_empty() { String::new() } else { format!(": {message}") })]
    Http { status: u16, message: String },
    /// A JSON-RPC error returned by the server.
    #[error("{message} (JSON-RPC error {code})")]
    Rpc { code: i64, message: String, data: Option<Value> },
    #[error("No response to \"{method}\" within {} s", .after.as_secs_f64())]
    Timeout { method: String, after: Duration },
    /// The connection is gone (process exited, session closed, ...).
    #[error("Connection closed: {0}")]
    Closed(String),
    /// The server sent something that is not valid MCP / JSON-RPC.
    #[error("Invalid response from server: {0}")]
    Protocol(String),
    /// The OAuth authorization flow failed (discovery, registration, login
    /// or token exchange).
    #[error("Authorization failed: {0}")]
    Auth(String),
    /// The user or the host cancelled (e.g. the browser login).
    #[error("{0}")]
    Cancelled(String),
}

impl McpError {
    /// The error a sampling or elicitation handler returns when the user
    /// declines (sent to the server as JSON-RPC error `-1`).
    pub fn rejected(message: impl Into<String>) -> McpError {
        McpError::Rpc { code: -1, message: message.into(), data: None }
    }

    /// JSON-RPC error code, for `Rpc` errors.
    pub fn rpc_code(&self) -> Option<i64> {
        match self {
            McpError::Rpc { code, .. } => Some(*code),
            _ => None,
        }
    }
}
