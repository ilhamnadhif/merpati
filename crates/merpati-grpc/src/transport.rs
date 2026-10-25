//! Connection setup: DNS + TCP (direct or tunnelled through the configured
//! proxy) + optional TLS (rustls/ring, ALPN h2), then an HTTP/2 tonic
//! [`Channel`] over that stream.
//!
//! The stream is established here rather than inside tonic so that errors
//! are precise ("connection refused", certificate problems, ...) and TLS can
//! skip verification, which tonic's own TLS config cannot do.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use hyper_util::rt::TokioIo;
use merpati_http::proxy::{ProxyKind, ProxyServer, ProxyStream};
use merpati_http::tls::TlsSetup;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tonic::transport::{Channel, Endpoint, Uri};

use crate::error::{GrpcError, error_chain};
use crate::target::{Address, Target};

/// Connect timeout when the target has no deadline.
pub(crate) const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

pub(crate) enum Stream {
    Plain(ProxyStream),
    Tls(Box<tokio_rustls::client::TlsStream<ProxyStream>>),
}

impl AsyncRead for Stream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_flush(cx),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_write_vectored(cx, bufs),
            Stream::Tls(s) => Pin::new(s.as_mut()).poll_write_vectored(cx, bufs),
        }
    }

    fn is_write_vectored(&self) -> bool {
        match self {
            Stream::Plain(s) => s.is_write_vectored(),
            Stream::Tls(s) => s.is_write_vectored(),
        }
    }
}

/// TLS settings of a connection, with the client certificate already
/// loaded, and the proxy the connection goes through.
#[derive(Clone, Debug)]
pub(crate) struct TlsOptions {
    pub verify: bool,
    /// Extra trusted roots (PEM) from the certificate settings.
    pub extra_roots: Option<Vec<u8>>,
    /// The client certificate configured for the host, if any.
    pub identity: Option<merpati_http::ClientIdentity>,
    /// Host pattern of the client certificate entry used, if any.
    pub client_cert_host: Option<String>,
    /// Set when the server asked for a client certificate we do not have.
    pub cert_requested: Arc<AtomicBool>,
    /// The proxy selected for the target by the proxy settings, if any.
    pub proxy: Option<ProxyServer>,
    /// Extra roots for TLS to an HTTPS proxy.
    pub proxy_tls: TlsSetup,
}

/// Records that the server asked for a client certificate (none configured).
#[derive(Debug)]
struct CertRequestRecorder(Arc<AtomicBool>);

impl rustls::client::ResolvesClientCert for CertRequestRecorder {
    fn resolve(&self, _: &[&[u8]], _: &[rustls::SignatureScheme]) -> Option<Arc<rustls::sign::CertifiedKey>> {
        self.0.store(true, Ordering::SeqCst);
        None
    }

    fn has_certs(&self) -> bool {
        false
    }
}

impl TlsOptions {
    /// Selects the proxy, reads the CA bundle and loads the client
    /// certificate configured for `addr` (TLS targets only), so that file,
    /// passphrase and proxy setting problems are reported before connecting.
    pub(crate) fn for_target(target: &Target, addr: &Address) -> Result<TlsOptions, GrpcError> {
        use merpati_http::certs;
        let proxy = merpati_http::proxy::proxy_for(&target.proxy, &addr.host, addr.port, addr.tls)
            .map_err(|e| GrpcError::Connect(e.message))?;
        let mut proxy_tls = TlsSetup::default();
        if target.verify_ssl && proxy.as_ref().is_some_and(|p| p.kind() == ProxyKind::Https) {
            proxy_tls.extra_roots_pem = certs::extra_roots(&target.certificates).map_err(GrpcError::Tls)?;
        }
        let mut opts = TlsOptions {
            verify: target.verify_ssl,
            extra_roots: None,
            identity: None,
            client_cert_host: None,
            cert_requested: Arc::default(),
            proxy,
            proxy_tls,
        };
        if !addr.tls {
            return Ok(opts);
        }
        if target.verify_ssl {
            opts.extra_roots = certs::extra_roots(&target.certificates).map_err(GrpcError::Tls)?;
        }
        if let Some(cert) = certs::find_client_certificate(&target.certificates, &addr.host, Some(addr.port)) {
            opts.client_cert_host = Some(cert.host.trim().to_string());
            let identity =
                certs::load_client_identity(cert, target.secret_resolver.as_ref()).map_err(GrpcError::Tls)?;
            opts.identity = Some(identity);
        }
        Ok(opts)
    }
}

/// Explains handshake failures caused by a missing or refused client
/// certificate; returns whether a hint was added.
pub(crate) fn client_cert_hint(msg: &mut String, addr: &Address, tls: &TlsOptions) -> bool {
    let lower = msg.to_ascii_lowercase();
    let alert = |name: &str| lower.contains(name);
    let hint = match &tls.client_cert_host {
        None if alert("certificaterequired") => {
            format!(" (the server requires a client certificate: add one for {} in Settings > Certificates)", addr.host)
        }
        // The handshake asked for one; the failure is most likely the server
        // dropping a connection without it.
        None if tls.cert_requested.load(Ordering::SeqCst) => format!(
            " (the server asked for a client certificate and none is configured for {}: add one in \
             Settings > Certificates if the server requires it)",
            addr.host
        ),
        Some(host)
            if alert("certificaterequired")
                || alert("badcertificate")
                || alert("unknownca")
                || alert("certificateunknown")
                || alert("decrypterror") =>
        {
            format!(" (the server rejected the client certificate configured for {host})")
        }
        _ => return false,
    };
    msg.push_str(&hint);
    true
}

/// Hands tonic the stream we already opened; reconnects (should tonic ever
/// ask) open a fresh one.
#[derive(Clone)]
struct Connector {
    ready: Arc<Mutex<Option<Stream>>>,
    addr: Address,
    tls: TlsOptions,
}

impl tower_service::Service<Uri> for Connector {
    type Response = TokioIo<Stream>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<TokioIo<Stream>>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _uri: Uri) -> Self::Future {
        let ready = self.ready.lock().ok().and_then(|mut g| g.take());
        let (addr, tls) = (self.addr.clone(), self.tls.clone());
        Box::pin(async move {
            match ready {
                Some(s) => Ok(TokioIo::new(s)),
                None => open_stream(&addr, &tls).await.map(TokioIo::new).map_err(io::Error::other),
            }
        })
    }
}

/// Opens TCP (+TLS) and performs the HTTP/2 handshake, within `timeout`.
pub(crate) async fn connect(addr: &Address, tls: &TlsOptions, timeout: Duration) -> Result<Channel, GrpcError> {
    let work = async {
        let stream = open_stream(addr, tls).await?;
        let scheme = if addr.tls { "https" } else { "http" };
        let origin: Uri = format!("{scheme}://{}", addr.authority)
            .parse()
            .map_err(|e| GrpcError::InvalidUrl { url: addr.authority.clone(), reason: format!("{e}") })?;
        // The endpoint URI is plain http so tonic never tries its own TLS;
        // the origin sets the real :scheme and :authority.
        let endpoint = Endpoint::from_shared(format!("http://{}", addr.authority))
            .map_err(|e| GrpcError::Connect(error_chain(&e)))?
            .origin(origin)
            .user_agent(concat!("merpati/", env!("CARGO_PKG_VERSION")))
            .map_err(|e| GrpcError::Connect(error_chain(&e)))?;
        let connector = Connector { ready: Arc::new(Mutex::new(Some(stream))), addr: addr.clone(), tls: tls.clone() };
        endpoint.connect_with_connector(connector).await.map_err(|e| {
            let mut msg = format!("HTTP/2 handshake with {} failed: {}", addr.authority, error_chain(&e));
            if addr.tls {
                client_cert_hint(&mut msg, addr, tls);
            }
            GrpcError::Connect(msg)
        })
    };
    match tokio::time::timeout(timeout, work).await {
        Ok(r) => r,
        Err(_) => Err(GrpcError::Timeout(format!(
            "Connecting to {} timed out after {}",
            addr.authority,
            format_duration(timeout)
        ))),
    }
}

async fn open_stream(addr: &Address, tls_opts: &TlsOptions) -> Result<Stream, GrpcError> {
    let tcp = match &tls_opts.proxy {
        Some(proxy) => proxy
            .tunnel(&addr.host, addr.port, &tls_opts.proxy_tls, tls_opts.verify)
            .await
            .map_err(|e| GrpcError::Connect(e.message))?,
        None => ProxyStream::Tcp(open_tcp(addr).await?),
    };
    if !addr.tls {
        return Ok(Stream::Plain(tcp));
    }
    tls_handshake(addr, tls_opts, tcp).await
}

async fn open_tcp(addr: &Address) -> Result<TcpStream, GrpcError> {
    let addrs: Vec<_> = tokio::net::lookup_host((addr.host.as_str(), addr.port))
        .await
        .map_err(|e| GrpcError::Connect(format!("Could not resolve host \"{}\": {e}", addr.host)))?
        .collect();
    if addrs.is_empty() {
        return Err(GrpcError::Connect(format!("Could not resolve host \"{}\"", addr.host)));
    }
    let mut last_err = None;
    let mut tcp = None;
    for sock in addrs {
        match TcpStream::connect(sock).await {
            Ok(s) => {
                tcp = Some(s);
                break;
            }
            Err(e) => last_err = Some(e),
        }
    }
    let tcp = match tcp {
        Some(t) => t,
        None => {
            let e = last_err.unwrap_or_else(|| io::Error::other("no address"));
            let msg = match e.kind() {
                io::ErrorKind::ConnectionRefused => {
                    format!("Connection refused: nothing is listening on {}", addr.authority)
                }
                _ => format!("Could not connect to {}: {e}", addr.authority),
            };
            return Err(GrpcError::Connect(msg));
        }
    };
    let _ = tcp.set_nodelay(true);
    Ok(tcp)
}

async fn tls_handshake(addr: &Address, tls_opts: &TlsOptions, tcp: ProxyStream) -> Result<Stream, GrpcError> {
    let verify = tls_opts.verify;
    let mut config =
        merpati_http::tls::client_config_with(verify, tls_opts.extra_roots.as_deref(), tls_opts.identity.as_ref())
            .map_err(GrpcError::Tls)?;
    config.alpn_protocols = vec![b"h2".to_vec()];
    if tls_opts.identity.is_none() {
        config.client_auth_cert_resolver = Arc::new(CertRequestRecorder(tls_opts.cert_requested.clone()));
    }
    let server_name = rustls::pki_types::ServerName::try_from(addr.host.clone())
        .map_err(|e| GrpcError::Tls(format!("invalid server name \"{}\": {e}", addr.host)))?;
    let tls = tokio_rustls::TlsConnector::from(Arc::new(config)).connect(server_name, tcp).await.map_err(|e| {
        let mut msg = format!("handshake with {} failed: {}", addr.authority, error_chain(&e));
        if !client_cert_hint(&mut msg, addr, tls_opts) {
            if verify && msg.contains("certificate") {
                msg.push_str(" (turn off SSL certificate verification to accept self-signed certificates)");
            } else if msg.contains("InvalidContentType") || msg.contains("corrupt message") {
                msg.push_str(" (the server may not use TLS; try grpc://)");
            }
        }
        GrpcError::Tls(msg)
    })?;
    if let Some(proto) = tls.get_ref().1.alpn_protocol()
        && proto != b"h2"
    {
        return Err(GrpcError::Tls(format!(
            "{} negotiated \"{}\" instead of HTTP/2 (h2); is this a gRPC endpoint?",
            addr.authority,
            String::from_utf8_lossy(proto)
        )));
    }
    Ok(Stream::Tls(Box::new(tls)))
}

pub(crate) fn format_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 || !ms.is_multiple_of(1000) { format!("{ms} ms") } else { format!("{} s", ms / 1000) }
}
