use std::net::SocketAddr;

/// Errors returned when starting the proxy or handling its certificate authority.
#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    #[error("could not listen on {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "refusing to listen on {0}: it is not a loopback address (set `allow_lan` to expose the proxy on the network)"
    )]
    NotLoopback(SocketAddr),
    #[error("certificate authority: {0}")]
    Ca(String),
    #[error("CA store: {0}")]
    CaStore(String),
    #[error("TLS configuration: {0}")]
    Tls(String),
    #[error("invalid upstream proxy: {0}")]
    UpstreamProxy(String),
}

impl From<rcgen::Error> for ProxyError {
    fn from(e: rcgen::Error) -> Self {
        ProxyError::Ca(e.to_string())
    }
}
