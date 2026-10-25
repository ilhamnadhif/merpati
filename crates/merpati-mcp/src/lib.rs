//! Model Context Protocol client for Merpati (FR-X8).
//!
//! Connects to local servers over stdio (a child process speaking
//! newline-delimited JSON-RPC) and to remote servers over Streamable HTTP
//! (JSON or SSE responses, `Mcp-Session-Id`) or the legacy HTTP+SSE
//! transport, signs in with OAuth (discovery, dynamic registration, PKCE),
//! lists and calls tools, resources and prompts, serves sampling,
//! elicitation and roots requests through host handlers, and exports the
//! server config for other MCP hosts.
//!
//! ```no_run
//! # async fn demo() -> Result<(), merpati_mcp::McpError> {
//! use merpati_mcp::{McpClient, McpConfig, Transport};
//! let cfg = McpConfig {
//!     transport: Transport::Http { url: "http://localhost:3000/mcp".into(), headers: vec![] },
//!     ..Default::default()
//! };
//! let client = McpClient::connect(cfg, |event| println!("{event:?}")).await?;
//! for tool in client.list_tools().await? {
//!     println!("{}: {:?}", tool.name, tool.description);
//! }
//! client.close().await;
//! # Ok(()) }
//! ```
//!
//! With OAuth sign-in and the client features (the host implements
//! [`OAuthStorage`] on its keychain, opens the browser, and asks the user
//! before answering sampling or elicitation requests):
//!
//! ```no_run
//! # async fn demo(cfg: merpati_mcp::McpConfig, storage: std::sync::Arc<dyn merpati_mcp::OAuthStorage>)
//! # -> Result<(), merpati_mcp::McpError> {
//! use merpati_mcp::*;
//! let cancel = CancellationToken::new(); // cancel() aborts the browser login
//! let oauth = OAuthOptions::new(|url| println!("open {url}")).with_storage(storage).with_cancel(cancel.clone());
//! let client = McpClient::builder(cfg)
//!     .oauth(oauth)
//!     .sampling(|req: SamplingRequest| async move {
//!         // Ask the user, run the model ...
//!         Ok(SamplingResult::text("some-model", format!("answer to {}", req.transcript())))
//!     })
//!     .elicitation(|req: ElicitationRequest| async move {
//!         let _form = req.fields(); // build a form from the requested schema
//!         Ok(ElicitationResponse::Decline)
//!     })
//!     .roots(vec![Root::new("file:///home/me/project", Some("project".into()))])
//!     .on_event(|e| println!("{e:?}"))
//!     .connect()
//!     .await?;
//! # client.close().await; Ok(()) }
//! ```

mod client;
mod config;
mod error;
mod export;
mod features;
mod http;
mod legacy;
mod oauth;
pub mod sse;
mod stdio;
mod types;

pub use client::{LATEST_PROTOCOL_VERSION, McpClient, McpClientBuilder, PROTOCOL_VERSIONS};
pub use config::{DEFAULT_TIMEOUT, McpConfig, McpHttpTransport, Transport};
pub use error::McpError;
pub use export::{args_template, export_config};
pub use features::{
    BoxFuture, ElicitField, ElicitFieldKind, ElicitationHandler, ElicitationRequest, ElicitationResponse, Root,
    SamplingHandler, SamplingMessage, SamplingRequest, SamplingResult,
};
pub use oauth::{
    DEFAULT_LOGIN_TIMEOUT, MemoryOAuthStorage, OAuthOptions, OAuthSettings, OAuthStorage, OAuthTokens,
    RegisteredClient, oauth_server_key,
};
pub use tokio_util::sync::CancellationToken;
pub use types::{
    CallResult, Content, Direction, ExportStyle, McpEvent, Prompt, PromptArgument, Resource, ResourceTemplate,
    ServerInfo, Tool,
};
