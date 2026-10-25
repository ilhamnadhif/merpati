//! Stream helpers: prefix replay, request-line sniffing, byte counting and
//! the bidirectional relay.

use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::{Buf, Bytes};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_util::sync::CancellationToken;

use crate::entry::StreamStats;
use crate::ws::WsSniffer;

/// Replays bytes that were already read (while sniffing the protocol)
/// before reading from the inner stream.
pub(crate) struct Rewind<T> {
    prefix: Bytes,
    inner: T,
}

impl<T> Rewind<T> {
    pub fn new(prefix: impl Into<Bytes>, inner: T) -> Self {
        Rewind { prefix: prefix.into(), inner }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for Rewind<T> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        if !self.prefix.is_empty() {
            let n = self.prefix.len().min(buf.remaining());
            buf.put_slice(&self.prefix[..n]);
            self.prefix.advance(n);
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Rewind<T> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

/// Longest request line looked at by [`OptionsSniffer`].
const MAX_SNIFFED_LINE: usize = 4096;
/// Request lines remembered per connection before the oldest are dropped.
const MAX_SNIFFED_TARGETS: usize = 64;

/// The absolute-form `OPTIONS` request targets a client sent on one
/// connection, in order: (authority, empty path and no query).
///
/// `OPTIONS http://host HTTP/1.1` must be forwarded as `OPTIONS *`
/// (RFC 9112 section 3.2.4), but the URI parser turns the empty path into
/// `/`, which then means the resource `/`. So the raw request lines are
/// sniffed off the client stream and matched to the parsed requests by
/// order. A match inside a body (a line that only looks like a request) is
/// noticed by its different authority and resets the queue, so the result is
/// then the parsed `/` again.
#[derive(Default)]
pub(crate) struct OptionsTargets {
    seen: parking_lot::Mutex<VecDeque<(String, bool)>>,
    off: AtomicBool,
}

impl OptionsTargets {
    /// Whether the next absolute-form `OPTIONS` request, to `authority`, had
    /// an empty path and no query.
    pub fn asterisk_form(&self, authority: &str) -> bool {
        let mut seen = self.seen.lock();
        match seen.pop_front() {
            Some((a, empty)) if a.eq_ignore_ascii_case(strip_userinfo(authority)) => empty,
            Some(_) => {
                seen.clear();
                false
            }
            None => false,
        }
    }

    /// Stops sniffing (the connection became a tunnel).
    pub fn stop(&self) {
        self.off.store(true, Ordering::Relaxed);
        self.seen.lock().clear();
    }

    fn push_line(&self, line: &[u8]) {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Ok(line) = std::str::from_utf8(line) else { return };
        let mut parts = line.split(' ');
        let (Some("OPTIONS"), Some(target), Some(version), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return;
        };
        let Some((_, rest)) = target.split_once("://").filter(|_| version.starts_with("HTTP/")) else { return };
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        let empty = authority.len() == rest.len();
        let mut seen = self.seen.lock();
        if seen.len() >= MAX_SNIFFED_TARGETS {
            seen.pop_front();
        }
        seen.push_back((strip_userinfo(authority).to_owned(), empty));
    }
}

fn strip_userinfo(authority: &str) -> &str {
    authority.rsplit_once('@').map_or(authority, |(_, host)| host)
}

/// Passes a client stream through while handing its `OPTIONS` request lines
/// to [`OptionsTargets`].
pub(crate) struct OptionsSniffer<T> {
    inner: T,
    targets: Arc<OptionsTargets>,
    /// The current line, while it may still be an `OPTIONS` request line.
    line: Vec<u8>,
    tracking: bool,
    at_line_start: bool,
}

impl<T> OptionsSniffer<T> {
    pub fn new(inner: T, targets: Arc<OptionsTargets>) -> Self {
        OptionsSniffer { inner, targets, line: Vec::new(), tracking: false, at_line_start: true }
    }

    fn feed(&mut self, mut data: &[u8]) {
        const PREFIX: &[u8] = b"OPTIONS ";
        while !data.is_empty() {
            if self.tracking {
                let end = data.iter().position(|&b| b == b'\n');
                let take = end.unwrap_or(data.len());
                self.line.extend_from_slice(&data[..take.min(MAX_SNIFFED_LINE + 1)]);
                data = &data[end.map_or(data.len(), |e| e + 1)..];
                let n = self.line.len().min(PREFIX.len());
                let viable = self.line[..n] == PREFIX[..n] && self.line.len() <= MAX_SNIFFED_LINE;
                if end.is_some() {
                    if viable {
                        self.targets.push_line(&self.line);
                    }
                    self.tracking = false;
                    self.at_line_start = true;
                } else if !viable {
                    self.tracking = false;
                    self.at_line_start = false;
                }
                if !self.tracking {
                    self.line.clear();
                }
            } else if self.at_line_start && data[0] == b'O' {
                self.tracking = true;
            } else {
                match data.iter().position(|&b| b == b'\n') {
                    Some(p) => {
                        data = &data[p + 1..];
                        self.at_line_start = true;
                    }
                    None => {
                        data = &[];
                        self.at_line_start = false;
                    }
                }
            }
        }
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for OptionsSniffer<T> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let this = &mut *self;
        let res = Pin::new(&mut this.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &res
            && !this.targets.off.load(Ordering::Relaxed)
        {
            this.feed(&buf.filled()[before..]);
        }
        res
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for OptionsSniffer<T> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

/// Counts bytes read from the inner stream and optionally feeds them to a
/// WebSocket sniffer.
struct Counted<T> {
    inner: T,
    count: Arc<AtomicU64>,
    sniffer: Option<WsSniffer>,
}

impl<T: AsyncRead + Unpin> AsyncRead for Counted<T> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let this = &mut *self;
        let res = Pin::new(&mut this.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &res {
            let new = &buf.filled()[before..];
            this.count.fetch_add(new.len() as u64, Ordering::Relaxed);
            if let Some(s) = this.sniffer.as_mut() {
                s.feed(new);
            }
        }
        res
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Counted<T> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

fn is_normal_close(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::NotConnected
    )
}

/// Copies both directions until both sides are done (or the proxy stops),
/// calling `on_tick` every `tick` when the counters moved. `sniffers` are
/// (client to server, server to client).
pub(crate) async fn relay<A, B>(
    client: A,
    upstream: B,
    token: &CancellationToken,
    sniffers: Option<(WsSniffer, WsSniffer)>,
    tick: Duration,
    mut on_tick: impl FnMut(&StreamStats),
) -> (StreamStats, Option<String>)
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let up = Arc::new(AtomicU64::new(0));
    let down = Arc::new(AtomicU64::new(0));
    let (client_sniffer, server_sniffer) = match sniffers {
        Some((c, s)) => (Some(c), Some(s)),
        None => (None, None),
    };
    let mut client = Counted { inner: client, count: up.clone(), sniffer: client_sniffer };
    let mut upstream = Counted { inner: upstream, count: down.clone(), sniffer: server_sniffer };
    let stats = || StreamStats { bytes_up: up.load(Ordering::Relaxed), bytes_down: down.load(Ordering::Relaxed) };
    let mut interval = tokio::time::interval(tick);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval.tick().await;
    let mut last = StreamStats::default();
    let copy = tokio::io::copy_bidirectional(&mut client, &mut upstream);
    tokio::pin!(copy);
    let error = loop {
        tokio::select! {
            r = &mut copy => break r.err().filter(|e| !is_normal_close(e)).map(|e| e.to_string()),
            _ = interval.tick() => {
                let now = stats();
                if now != last {
                    on_tick(&now);
                    last = now;
                }
            }
            _ = token.cancelled() => break Some("the proxy was stopped".to_owned()),
        }
    };
    (stats(), error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sniff(chunks: &[&[u8]]) -> Vec<(String, bool)> {
        let targets = Arc::new(OptionsTargets::default());
        let mut s = OptionsSniffer::new((), targets.clone());
        for c in chunks {
            s.feed(c);
        }
        targets.seen.lock().iter().cloned().collect()
    }

    #[test]
    fn sniffs_absolute_form_options_lines() {
        let got = sniff(&[
            b"OPTIONS http://a.test HTTP/1.1\r\nHost: a.test\r\n\r\n",
            b"OPTIONS http://u:p@b.test:81/ HTTP/1.1\r\n\r\nOPTIONS http://c.test?x HTTP/1.1\r\n\r\n",
            b"GET http://d.test HTTP/1.1\r\n\r\nOPTIONS * HTTP/1.1\r\n\r\n",
        ]);
        let want = [("a.test", true), ("b.test:81", false), ("c.test", false)];
        assert_eq!(got, want.map(|(a, e)| (a.to_owned(), e)));

        // Split anywhere, also inside "OPTIONS".
        let text = b"POST http://x/ HTTP/1.1\r\nContent-Length: 0\r\n\r\nOPTIONS https://e.test:8443 HTTP/1.1\r\n\r\n";
        for cut in 1..text.len() {
            let got = sniff(&[&text[..cut], &text[cut..]]);
            assert_eq!(got, vec![("e.test:8443".to_owned(), true)], "cut at {cut}");
        }
        // Not at a line start, too long, or not a request line.
        assert!(sniff(&[b"xOPTIONS http://a HTTP/1.1\r\n"]).is_empty());
        assert!(sniff(&[b"OPTIONS http://a HTTP/1.1 extra\r\n", b"OPTIONSX http://a HTTP/1.1\r\n"]).is_empty());
        let long = format!("OPTIONS http://{} HTTP/1.1\r\n", "a".repeat(MAX_SNIFFED_LINE));
        assert!(sniff(&[long.as_bytes()]).is_empty());
    }

    #[test]
    fn targets_match_requests_in_order() {
        let t = OptionsTargets::default();
        t.push_line(b"OPTIONS http://a.test HTTP/1.1\r");
        t.push_line(b"OPTIONS http://a.test/ HTTP/1.1\r");
        assert!(t.asterisk_form("A.test"));
        assert!(!t.asterisk_form("a.test"));
        assert!(!t.asterisk_form("a.test"));
        // Out of step (a line inside a body): start over with the parsed path.
        t.push_line(b"OPTIONS http://body.test HTTP/1.1");
        t.push_line(b"OPTIONS http://a.test HTTP/1.1");
        assert!(!t.asterisk_form("a.test"));
        assert!(!t.asterisk_form("a.test"));
        t.push_line(b"OPTIONS http://a.test HTTP/1.1");
        t.stop();
        assert!(!t.asterisk_form("a.test"));
    }
}
