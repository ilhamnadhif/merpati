//! Running calls: the event-reporting [`GrpcCall`] handle used by the UI for
//! all four method kinds, and the collecting [`invoke`] / [`invoke_unary`].

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{FutureExt as _, StreamExt as _};
use prost_reflect::{DynamicMessage, MessageDescriptor, MethodDescriptor};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::metadata::MetadataMap;

use crate::codec::DynamicCodec;
use crate::error::{GrpcError, status_code_name, status_error, transport_failure};
use crate::schema::{GrpcSchema, MethodKind, message_to_json, method_kind, parse_message};
use crate::target::{Address, Target, build_metadata, metadata_pairs, parse_address};
use crate::transport::{DEFAULT_CONNECT_TIMEOUT, TlsOptions, client_cert_hint, connect, format_duration};

/// Largest response message accepted (tonic's default is 4 MiB).
const MAX_DECODE_SIZE: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Direction {
    Sent,
    Received,
}

/// What happens during a call, in order. Every call ends with exactly one
/// `Finished`; `Error` precedes it when the failure happened on the client
/// side (connect, TLS, transport, decoding) rather than being a status sent
/// by the server.
#[derive(Clone, Debug, PartialEq)]
pub enum GrpcEvent {
    Connecting,
    ResponseHeaders(Vec<(String, String)>),
    /// `json` is the message as protobuf-JSON (pretty, default values included).
    Message {
        direction: Direction,
        json: String,
        at_ms: i64,
    },
    Trailers(Vec<(String, String)>),
    Finished {
        code: i32,
        code_name: String,
        message: String,
        elapsed_ms: f64,
    },
    Error(String),
}

type EventFn = Arc<dyn Fn(GrpcEvent) + Send + Sync>;

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Everything a call needs, validated up front.
struct CallSpec {
    addr: Address,
    tls: TlsOptions,
    metadata: MetadataMap,
    timeout: Option<Duration>,
    method: MethodDescriptor,
}

fn prepare(target: &Target, schema: &GrpcSchema, service: &str, method: &str) -> Result<CallSpec, GrpcError> {
    let method = schema.method_descriptor(service, method)?;
    let addr = parse_address(&target.url, target.tls)?;
    let metadata = build_metadata(&target.metadata)?;
    Ok(CallSpec { tls: TlsOptions::for_target(target, &addr)?, addr, metadata, timeout: target.timeout, method })
}

/// Handle to a running call. Dropping it cancels the call.
pub struct GrpcCall {
    tx: Mutex<Option<mpsc::UnboundedSender<DynamicMessage>>>,
    cancel: CancellationToken,
    input: MessageDescriptor,
    kind: MethodKind,
    method_name: String,
    finished: Arc<AtomicBool>,
}

impl GrpcCall {
    /// Starts any kind of call on `rt`; progress is reported through
    /// `on_event` (called from a runtime thread).
    ///
    /// For unary / server-streaming methods `first_message` is the request
    /// (`None` or blank = empty message). For client / bidi streaming it is
    /// sent first if `Some`, and more can follow with [`GrpcCall::send`]
    /// until [`GrpcCall::end_stream`].
    ///
    /// Fails without connecting for an unknown service/method, invalid
    /// URL or metadata, or a first message that does not match the input type.
    pub fn start(
        rt: &tokio::runtime::Handle,
        target: Target,
        schema: Arc<GrpcSchema>,
        service: &str,
        method: &str,
        first_message: Option<String>,
        on_event: impl Fn(GrpcEvent) + Send + Sync + 'static,
    ) -> Result<GrpcCall, GrpcError> {
        let spec = prepare(&target, &schema, service, method)?;
        let input = spec.method.input();
        let kind = method_kind(&spec.method);
        let first = match first_message {
            Some(json) => Some(parse_message(&input, &json)?),
            None if kind.is_client_streaming() => None,
            None => Some(DynamicMessage::new(input.clone())),
        };
        let (tx, rx) = mpsc::unbounded_channel();
        if let Some(msg) = first {
            let _ = tx.send(msg);
        }
        // Single-request methods: dropping the sender ends the request stream.
        let tx = kind.is_client_streaming().then_some(tx);
        let cancel = CancellationToken::new();
        let finished = Arc::new(AtomicBool::new(false));
        let on_event: EventFn = Arc::new(on_event);
        let method_name = spec.method.full_name().to_string();
        {
            let cancel = cancel.clone();
            let finished = finished.clone();
            rt.spawn(async move {
                let started = Instant::now();
                let outcome = AssertUnwindSafe(run(spec, rx, cancel, on_event.clone())).catch_unwind().await;
                if outcome.is_err() {
                    let msg = "internal error: the call task panicked".to_string();
                    on_event(GrpcEvent::Error(msg.clone()));
                    on_event(finished_event(13, msg, started));
                }
                finished.store(true, Ordering::SeqCst);
            });
        }
        Ok(GrpcCall { tx: Mutex::new(tx), cancel, input, kind, method_name, finished })
    }

    /// Sends another request message (client / bidi streaming only). The
    /// JSON is validated against the input type first.
    pub fn send(&self, json: &str) -> Result<(), GrpcError> {
        if !self.kind.is_client_streaming() {
            return Err(GrpcError::NotClientStreaming(self.method_name.clone()));
        }
        let msg = parse_message(&self.input, json)?;
        let guard = self.tx.lock().map_err(|_| GrpcError::StreamClosed)?;
        match guard.as_ref() {
            Some(tx) => tx.send(msg).map_err(|_| GrpcError::StreamClosed),
            None => Err(GrpcError::StreamClosed),
        }
    }

    /// Half-closes the request stream: the server sees the end of the
    /// client's messages. No-op for single-request methods.
    pub fn end_stream(&self) {
        if let Ok(mut guard) = self.tx.lock() {
            guard.take();
        }
    }

    /// Aborts the call (`Finished` with CANCELLED follows unless it already ended).
    pub fn cancel(&self) {
        self.end_stream();
        self.cancel.cancel();
    }

    pub fn kind(&self) -> MethodKind {
        self.kind
    }

    /// Whether more messages can still be sent.
    pub fn can_send(&self) -> bool {
        !self.is_finished() && self.tx.lock().map(|g| g.as_ref().is_some_and(|tx| !tx.is_closed())).unwrap_or(false)
    }

    /// The call has ended (its `Finished` event was emitted).
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::SeqCst)
    }
}

impl Drop for GrpcCall {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn finished_event(code: i32, message: String, started: Instant) -> GrpcEvent {
    GrpcEvent::Finished {
        code,
        code_name: status_code_name(code).to_string(),
        message,
        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
    }
}

/// Runs one call to completion, emitting events. Returns `Ok` for status
/// OK, `Err(GrpcError::Status)` for any other status, or the client-side
/// error (connect, TLS, timeout).
async fn run(
    spec: CallSpec,
    rx: mpsc::UnboundedReceiver<DynamicMessage>,
    cancel: CancellationToken,
    on_event: EventFn,
) -> Result<(), GrpcError> {
    let started = Instant::now();
    let deadline = spec.timeout.map(|t| started + t);
    let finish = |code: i32, message: String, metadata: Vec<(String, String)>| -> Result<(), GrpcError> {
        on_event(finished_event(code, message.clone(), started));
        if code == 0 { Ok(()) } else { Err(status_error(code, message, metadata)) }
    };
    let cancelled = || "Cancelled by client".to_string();
    let tls = spec.addr.tls;
    let deadline_msg = || format!("Deadline of {} exceeded", format_duration(spec.timeout.unwrap_or_default()));

    on_event(GrpcEvent::Connecting);
    let connect_timeout = spec.timeout.unwrap_or(DEFAULT_CONNECT_TIMEOUT);
    let channel = tokio::select! {
        r = connect(&spec.addr, &spec.tls, connect_timeout) => r,
        _ = cancel.cancelled() => return finish(1, cancelled(), vec![]),
    };
    let channel = match channel {
        Ok(c) => c,
        Err(e) => {
            let code = match (&e, spec.timeout) {
                (GrpcError::Timeout(_), Some(_)) => 4,
                _ => 14,
            };
            on_event(GrpcEvent::Error(e.to_string()));
            on_event(finished_event(code, e.to_string(), started));
            return Err(e);
        }
    };

    let sent = on_event.clone();
    let requests = UnboundedReceiverStream::new(rx).map(move |msg| {
        sent(GrpcEvent::Message { direction: Direction::Sent, json: message_to_json(&msg), at_ms: now_ms() });
        msg
    });
    let mut request = tonic::Request::new(requests);
    *request.metadata_mut() = spec.metadata.clone();
    if let Some(d) = deadline {
        request.set_timeout(d.saturating_duration_since(Instant::now()));
    }
    let service = spec.method.parent_service();
    let path: http::uri::PathAndQuery =
        format!("/{}/{}", service.full_name(), spec.method.name()).parse().map_err(|_| GrpcError::UnknownMethod {
            service: service.full_name().into(),
            method: spec.method.name().into(),
        })?;
    let decode_error = Arc::new(Mutex::new(None));
    let codec = DynamicCodec { output: spec.method.output(), decode_error: decode_error.clone() };
    let mut grpc = tonic::client::Grpc::new(channel).max_decoding_message_size(MAX_DECODE_SIZE);

    let deadline_sleep = async move {
        match deadline {
            Some(d) => tokio::time::sleep_until(d).await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(deadline_sleep);

    // Reports a non-OK status from tonic.
    let report = |status: tonic::Status| -> Result<(), GrpcError> {
        let mut code = status.code() as i32;
        let mut message = status.message().to_string();
        let source = std::error::Error::source(&status);
        if source.is_some_and(is_timeout) {
            // tonic reports its own client-side deadline as CANCELLED.
            code = 4;
            message = deadline_msg();
        } else if let Some(src) = source {
            // Our own cancellation never goes through tonic, so CANCELLED /
            // UNKNOWN with a source means the transport failed.
            if matches!(code, 1 | 2) {
                code = 14;
            }
            message = transport_failure(src, tls);
            if tls {
                client_cert_hint(&mut message, &spec.addr, &spec.tls);
            }
            on_event(GrpcEvent::Error(message.clone()));
        } else if let Some(err) = decode_error.lock().ok().and_then(|mut g| g.take()) {
            on_event(GrpcEvent::Error(err));
        }
        let trailers = if source.is_none() { metadata_pairs(status.metadata()) } else { vec![] };
        if source.is_none() {
            on_event(GrpcEvent::Trailers(trailers.clone()));
        }
        finish(code, message, trailers)
    };

    let response = tokio::select! {
        r = async {
            grpc.ready().await.map_err(|e| tonic::Status::from_error(Box::new(e)))?;
            grpc.streaming(request, path, codec).await
        } => r,
        _ = cancel.cancelled() => return finish(1, cancelled(), vec![]),
        _ = &mut deadline_sleep => return finish(4, deadline_msg(), vec![]),
    };
    let mut stream = match response {
        Ok(resp) => {
            let (headers, stream, _) = resp.into_parts();
            on_event(GrpcEvent::ResponseHeaders(metadata_pairs(&headers)));
            stream
        }
        Err(status) => return report(status),
    };
    loop {
        tokio::select! {
            m = stream.message() => match m {
                Ok(Some(msg)) => on_event(GrpcEvent::Message {
                    direction: Direction::Received,
                    json: message_to_json(&msg),
                    at_ms: now_ms(),
                }),
                Ok(None) => break,
                Err(status) => return report(status),
            },
            _ = cancel.cancelled() => return finish(1, cancelled(), vec![]),
            _ = &mut deadline_sleep => return finish(4, deadline_msg(), vec![]),
        }
    }
    let trailers = match stream.trailers().await {
        Ok(Some(map)) => {
            // A grpc-status of 0 in the trailers; other codes surfaced above.
            let message = map.get("grpc-message").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
            (metadata_pairs(&map), message)
        }
        Ok(None) => (vec![], String::new()),
        Err(status) => return report(status),
    };
    on_event(GrpcEvent::Trailers(trailers.0.clone()));
    finish(0, trailers.1, trailers.0)
}

fn is_timeout(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut cur = Some(err);
    while let Some(e) = cur {
        if e.is::<tonic::TimeoutExpired>() {
            return true;
        }
        cur = e.source();
    }
    false
}

// ---------------------------------------------------------------------------
// Collecting calls
// ---------------------------------------------------------------------------

/// Outcome of [`invoke`]: everything the server sent back.
#[derive(Clone, Debug, PartialEq)]
pub struct CallResult {
    /// Response messages as protobuf-JSON.
    pub messages: Vec<String>,
    pub code: i32,
    pub code_name: String,
    pub message: String,
    pub headers: Vec<(String, String)>,
    pub trailers: Vec<(String, String)>,
    pub elapsed_ms: f64,
}

/// Result of [`invoke_unary`].
#[derive(Clone, Debug, PartialEq)]
pub struct UnaryResponse {
    /// The response message as protobuf-JSON.
    pub json: String,
    /// Always 0 / `OK` (other statuses are returned as [`GrpcError::Status`]).
    pub code: i32,
    pub code_name: String,
    pub headers: Vec<(String, String)>,
    pub trailers: Vec<(String, String)>,
    pub elapsed_ms: f64,
}

/// Runs a call of any kind with a fixed list of request messages (all sent,
/// then the stream is half-closed) and collects the responses. A non-OK
/// status is reported in the result; `Err` means the call could not be
/// made (bad input, connection or TLS failure, ...).
pub async fn invoke(
    target: &Target,
    schema: &GrpcSchema,
    service: &str,
    method: &str,
    messages: &[String],
) -> Result<CallResult, GrpcError> {
    let spec = prepare(target, schema, service, method)?;
    let input = spec.method.input();
    let (tx, rx) = mpsc::unbounded_channel();
    for json in messages {
        let _ = tx.send(parse_message(&input, json)?);
    }
    if messages.is_empty() && !method_kind(&spec.method).is_client_streaming() {
        let _ = tx.send(DynamicMessage::new(input));
    }
    drop(tx);
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let on_event: EventFn = Arc::new(move |e| {
        if let Ok(mut v) = sink.lock() {
            v.push(e);
        }
    });
    let outcome = run(spec, rx, CancellationToken::new(), on_event).await;
    if let Err(e) = outcome
        && !matches!(e, GrpcError::Status { .. })
    {
        return Err(e);
    }
    let events = std::mem::take(&mut *events.lock().map_err(|_| GrpcError::Other("event log poisoned".into()))?);
    let mut result = CallResult {
        messages: Vec::new(),
        code: 2,
        code_name: String::new(),
        message: String::new(),
        headers: Vec::new(),
        trailers: Vec::new(),
        elapsed_ms: 0.0,
    };
    for e in events {
        match e {
            GrpcEvent::ResponseHeaders(h) => result.headers = h,
            GrpcEvent::Message { direction: Direction::Received, json, .. } => result.messages.push(json),
            GrpcEvent::Trailers(t) => result.trailers = t,
            GrpcEvent::Finished { code, code_name, message, elapsed_ms } => {
                result.code = code;
                result.code_name = code_name;
                result.message = message;
                result.elapsed_ms = elapsed_ms;
            }
            _ => {}
        }
    }
    Ok(result)
}

/// Unary call returning the response message, status and metadata. A
/// non-OK status is `Err(GrpcError::Status)` (carrying the trailers).
pub async fn invoke_unary(
    target: &Target,
    schema: &GrpcSchema,
    service: &str,
    method: &str,
    json: &str,
) -> Result<UnaryResponse, GrpcError> {
    let desc = schema.method_descriptor(service, method)?;
    let kind = method_kind(&desc);
    if kind != MethodKind::Unary {
        return Err(GrpcError::NotUnary { method: desc.full_name().to_string(), kind: kind.label().to_string() });
    }
    let r = invoke(target, schema, service, method, &[json.to_string()]).await?;
    if r.code != 0 {
        return Err(GrpcError::Status {
            code: r.code,
            code_name: r.code_name,
            message: r.message,
            metadata: r.trailers,
        });
    }
    let json =
        r.messages.into_iter().next().ok_or_else(|| status_error(13, "the server sent no response message", vec![]))?;
    Ok(UnaryResponse {
        json,
        code: r.code,
        code_name: r.code_name,
        headers: r.headers,
        trailers: r.trailers,
        elapsed_ms: r.elapsed_ms,
    })
}
