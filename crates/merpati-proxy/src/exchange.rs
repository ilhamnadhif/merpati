//! In-flight capture of one exchange, and the body wrapper that copies a
//! capped prefix of each streamed body into it.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use bytes::{Bytes, BytesMut};
use http::HeaderMap;
use http_body::{Body, Frame, SizeHint};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use parking_lot::Mutex;

use crate::config::CaptureFilter;
use crate::entry::{CaptureEntry, CapturedBody, EntryState};
use crate::store::Store;

pub(crate) type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub(crate) type ProxyBody = UnsyncBoxBody<Bytes, BoxError>;

pub(crate) fn empty() -> ProxyBody {
    Empty::<Bytes>::new().map_err(|never| match never {}).boxed_unsync()
}

pub(crate) fn full(data: impl Into<Bytes>) -> ProxyBody {
    Full::new(data.into()).map_err(|never| match never {}).boxed_unsync()
}

pub(crate) fn headers_vec(headers: &HeaderMap) -> Vec<(String, String)> {
    headers.iter().map(|(k, v)| (k.as_str().to_owned(), String::from_utf8_lossy(v.as_bytes()).into_owned())).collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Side {
    Request,
    Response,
}

/// Keeps the first `cap` bytes of a stream and counts the rest.
struct Recorder {
    buf: BytesMut,
    cap: usize,
    size: u64,
    truncated: bool,
}

impl Recorder {
    fn new(cap: usize) -> Self {
        Recorder { buf: BytesMut::new(), cap, size: 0, truncated: false }
    }

    /// How many of `len` new bytes fit under the cap.
    fn room(&self, len: usize) -> usize {
        len.min(self.cap.saturating_sub(self.buf.len()))
    }

    /// Counts `data` and keeps its first `keep` bytes.
    fn push(&mut self, data: &[u8], keep: usize) {
        self.size += data.len() as u64;
        if keep < data.len() {
            self.truncated = true;
        }
        self.buf.extend_from_slice(&data[..keep]);
    }

    fn snapshot(&self) -> CapturedBody {
        CapturedBody { data: Bytes::copy_from_slice(&self.buf), size: self.size, truncated: self.truncated }
    }

    fn take(&mut self) -> CapturedBody {
        CapturedBody { data: self.buf.split().freeze(), size: self.size, truncated: self.truncated }
    }
}

pub(crate) struct Exchange {
    store: Arc<Store>,
    started: Instant,
    inner: Mutex<Inner>,
}

struct Inner {
    entry: CaptureEntry,
    req: Recorder,
    resp: Recorder,
    published: bool,
    /// Set when a content-type filter decides visibility at response time.
    deferred_filter: Option<CaptureFilter>,
    discarded: bool,
    finished: bool,
}

impl Inner {
    fn snapshot(&self) -> CaptureEntry {
        let mut e = self.entry.clone();
        e.request_body = self.req.snapshot();
        e.response_body = self.resp.snapshot();
        e
    }

    /// Body bytes buffered (reserved in the store as in flight).
    fn buffered(&self) -> usize {
        self.req.buf.len() + self.resp.buf.len()
    }
}

impl Exchange {
    /// Starts capturing `entry`. It is published right away unless `filter`
    /// restricts content types, in which case it appears with its response.
    pub fn begin(store: Arc<Store>, entry: CaptureEntry, max_body: usize, filter: &CaptureFilter) -> Arc<Self> {
        let deferred = filter.has_content_types();
        if !deferred {
            store.insert(entry.clone());
        }
        Arc::new(Exchange {
            store,
            started: Instant::now(),
            inner: Mutex::new(Inner {
                entry,
                req: Recorder::new(max_body),
                resp: Recorder::new(max_body),
                published: !deferred,
                deferred_filter: deferred.then(|| filter.clone()),
                discarded: false,
                finished: false,
            }),
        })
    }

    /// Copies what fits of a body chunk. The buffered bytes count toward the
    /// store's total budget while the body streams, not only once finished.
    fn record(&self, side: Side, data: &[u8]) {
        let mut inner = self.inner.lock();
        if inner.finished || inner.discarded {
            return;
        }
        let id = inner.entry.id;
        let recorder = match side {
            Side::Request => &mut inner.req,
            Side::Response => &mut inner.resp,
        };
        let want = recorder.room(data.len());
        let keep = if want > 0 { self.store.reserve(id, want) } else { 0 };
        recorder.push(data, keep);
    }

    fn trailers(&self, side: Side, trailers: &HeaderMap) {
        if side == Side::Response {
            self.inner.lock().entry.response_trailers = headers_vec(trailers);
        }
    }

    fn body_end(&self, side: Side, error: Option<String>) {
        match side {
            Side::Response => self.finish(error),
            Side::Request => {
                if let Some(e) = error {
                    let mut inner = self.inner.lock();
                    if !inner.finished && inner.entry.note.is_none() {
                        inner.entry.note = Some(format!("request body: {e}"));
                    }
                }
            }
        }
    }

    /// Response headers arrived: fills the entry and publishes it (or drops
    /// it when the content-type filter rejects it).
    pub fn response_head(&self, status: u16, headers: &HeaderMap, upstream_version: &str, timings: crate::Timings) {
        let (snapshot, was_published) = {
            let mut inner = self.inner.lock();
            if inner.finished {
                return;
            }
            inner.entry.status = Some(status);
            inner.entry.response_headers = headers_vec(headers);
            inner.entry.upstream_http_version = Some(upstream_version.to_owned());
            inner.entry.timings = timings;
            inner.entry.state = EntryState::Receiving;
            if let Some(f) = &inner.deferred_filter
                && !f.allows_content_type(inner.entry.response_header("content-type"))
            {
                inner.discarded = true;
                let freed = inner.buffered();
                inner.req = Recorder::new(0);
                inner.resp = Recorder::new(0);
                drop(inner);
                self.store.release(freed);
                return;
            }
            let was = inner.published;
            inner.published = true;
            (inner.snapshot(), was)
        };
        if was_published {
            self.store.update(snapshot);
        } else {
            self.store.insert(snapshot);
        }
    }

    /// Changes the entry and publishes the change (if it is visible).
    pub fn update(&self, f: impl FnOnce(&mut CaptureEntry)) {
        let snapshot = {
            let mut inner = self.inner.lock();
            if inner.finished {
                return;
            }
            f(&mut inner.entry);
            if inner.discarded || !inner.published {
                return;
            }
            inner.snapshot()
        };
        self.store.update(snapshot);
    }

    /// Finalizes the entry: bodies, total time, state. Later calls are ignored.
    pub fn finish(&self, error: Option<String>) {
        let (entry, was_published, buffered) = {
            let mut inner = self.inner.lock();
            if inner.finished {
                return;
            }
            inner.finished = true;
            inner.entry.timings.total = Some(self.started.elapsed());
            if error.is_some() {
                inner.entry.error = error;
            }
            inner.entry.state = if inner.entry.error.is_some() { EntryState::Failed } else { EntryState::Complete };
            let buffered = inner.buffered();
            let hidden = inner.discarded
                || (!inner.published
                    && inner
                        .deferred_filter
                        .as_ref()
                        .is_some_and(|f| !f.allows_content_type(inner.entry.content_type())));
            if hidden {
                inner.req = Recorder::new(0);
                inner.resp = Recorder::new(0);
                drop(inner);
                self.store.release(buffered);
                return;
            }
            let req = inner.req.take();
            let resp = inner.resp.take();
            inner.entry.request_body = req;
            inner.entry.response_body = resp;
            (inner.entry.clone(), inner.published, buffered)
        };
        // The buffers move into the stored entry: stored, no longer in flight.
        self.store.put(entry, !was_published, buffered);
    }

    /// Finalizes with an error.
    pub fn fail(&self, error: impl Into<String>) {
        self.finish(Some(error.into()));
    }
}

impl Drop for Exchange {
    /// An exchange dropped before it finished was aborted: the client went
    /// away while waiting for the server, or the proxy stopped.
    fn drop(&mut self) {
        self.finish(Some("aborted: the connection closed before the exchange finished".into()));
    }
}

/// Passes a hyper body through while copying a capped prefix into an
/// [`Exchange`]. Nothing beyond `max_body_bytes` is buffered.
pub(crate) struct TeeBody {
    inner: Incoming,
    side: Side,
    exchange: Arc<Exchange>,
    done: bool,
}

impl TeeBody {
    pub fn boxed(inner: Incoming, side: Side, exchange: Option<&Arc<Exchange>>) -> ProxyBody {
        match exchange {
            Some(ex) => TeeBody { inner, side, exchange: ex.clone(), done: false }.boxed_unsync(),
            None => inner.map_err(|e| Box::new(e) as BoxError).boxed_unsync(),
        }
    }
}

impl Body for TeeBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = &mut *self;
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.exchange.record(this.side, data);
                } else if let Some(trailers) = frame.trailers_ref() {
                    this.exchange.trailers(this.side, trailers);
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(e))) => {
                this.done = true;
                let who = if this.side == Side::Response { "upstream" } else { "client" };
                this.exchange.body_end(this.side, Some(format!("{who} body error: {e}")));
                Poll::Ready(Some(Err(Box::new(e))))
            }
            Poll::Ready(None) => {
                this.done = true;
                this.exchange.body_end(this.side, None);
                Poll::Ready(None)
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for TeeBody {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let error = (!self.inner.is_end_stream()).then(|| match self.side {
            Side::Request => "the request body was not fully sent".to_owned(),
            Side::Response => "the client closed the connection before the response finished".to_owned(),
        });
        self.exchange.body_end(self.side, error);
    }
}
