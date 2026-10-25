//! Incremental parser for `text/event-stream` bodies (WHATWG server-sent
//! events). Shared by the Streamable HTTP transport and the AI providers.
//!
//! A server must not be able to exhaust memory with an endless line or
//! event: the parser holds at most [`SseParser::limit`] bytes of a line and
//! of an event's `data`, and fails with [`SseTooLarge`] beyond that.

/// Default largest line and event `data` the parser holds (64 MiB).
pub const MAX_EVENT_BYTES: usize = 64 * 1024 * 1024;

/// A line or event was larger than the parser's limit; the stream should be
/// abandoned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the server sent an event larger than {}", size_text(*.limit))]
pub struct SseTooLarge {
    pub limit: usize,
}

/// `64 MiB`, or a byte count for small sizes.
pub fn size_text(bytes: usize) -> String {
    const MIB: usize = 1024 * 1024;
    if bytes >= MIB && bytes.is_multiple_of(MIB) { format!("{} MiB", bytes / MIB) } else { format!("{bytes} bytes") }
}

/// One dispatched event.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SseEvent {
    /// The `event:` field; `"message"` when the server sent none.
    pub event: String,
    /// The joined `data:` lines (without the trailing newline).
    pub data: String,
    pub id: Option<String>,
    pub retry: Option<u64>,
}

/// Feed it chunks as they arrive; it returns the events completed so far.
/// Handles `\n`, `\r\n` and `\r` line endings split across chunks.
#[derive(Debug)]
pub struct SseParser {
    limit: usize,
    line: Vec<u8>,
    skip_lf: bool,
    started: bool,
    /// The first bytes, held back while they may be a BOM split across chunks.
    head: Vec<u8>,
    event: String,
    data: String,
    has_data: bool,
    id: Option<String>,
    retry: Option<u64>,
    last_id: Option<String>,
    /// The stream's reconnection time from the last valid `retry:` field.
    reconnect_ms: Option<u64>,
}

impl Default for SseParser {
    fn default() -> Self {
        SseParser::with_limit(MAX_EVENT_BYTES)
    }
}

impl SseParser {
    /// A parser with the default limit ([`MAX_EVENT_BYTES`]).
    pub fn new() -> Self {
        SseParser::default()
    }

    /// A parser that fails once a line or an event's data exceeds `limit` bytes.
    pub fn with_limit(limit: usize) -> Self {
        SseParser {
            limit,
            line: Vec::new(),
            skip_lf: false,
            started: false,
            head: Vec::new(),
            event: String::new(),
            data: String::new(),
            has_data: false,
            id: None,
            retry: None,
            last_id: None,
            reconnect_ms: None,
        }
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    /// The most recent `id:` seen (for `Last-Event-ID` on reconnect).
    pub fn last_event_id(&self) -> Option<&str> {
        self.last_id.as_deref()
    }

    /// The reconnection time (ms) the server asked for with `retry:`, set as
    /// soon as the field is read (also by events that carry no data).
    pub fn retry_ms(&self) -> Option<u64> {
        self.reconnect_ms
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<SseEvent>, SseTooLarge> {
        const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];
        let mut out = Vec::new();
        let head;
        let mut bytes = chunk;
        if !self.started && !bytes.is_empty() {
            self.head.extend_from_slice(bytes);
            if self.head.len() < BOM.len() && BOM.starts_with(&self.head) {
                return Ok(out);
            }
            self.started = true;
            head = std::mem::take(&mut self.head);
            bytes = head.strip_prefix(&BOM[..]).unwrap_or(&head);
        }
        for &b in bytes {
            match b {
                b'\n' if self.skip_lf => self.skip_lf = false,
                b'\n' => self.end_line(&mut out)?,
                b'\r' => {
                    self.end_line(&mut out)?;
                    self.skip_lf = true;
                }
                _ => {
                    self.skip_lf = false;
                    if self.line.len() >= self.limit {
                        self.line.clear();
                        return Err(SseTooLarge { limit: self.limit });
                    }
                    self.line.push(b);
                }
            }
        }
        Ok(out)
    }

    /// Flushes a final event that was not terminated by a blank line.
    pub fn finish(&mut self) -> Result<Option<SseEvent>, SseTooLarge> {
        let mut out = Vec::new();
        if !self.line.is_empty() {
            self.end_line(&mut out)?;
        }
        self.end_line(&mut out)?;
        Ok(out.pop())
    }

    fn end_line(&mut self, out: &mut Vec<SseEvent>) -> Result<(), SseTooLarge> {
        let line = std::mem::take(&mut self.line);
        if line.is_empty() {
            if self.has_data {
                let mut data = std::mem::take(&mut self.data);
                if data.ends_with('\n') {
                    data.pop();
                }
                let event = std::mem::take(&mut self.event);
                out.push(SseEvent {
                    event: if event.is_empty() { "message".into() } else { event },
                    data,
                    id: self.id.clone(),
                    retry: self.retry.take(),
                });
            } else {
                self.event.clear();
                self.data.clear();
            }
            self.has_data = false;
            return Ok(());
        }
        if line[0] == b':' {
            return Ok(());
        }
        let line = String::from_utf8_lossy(&line);
        let (field, value) = match line.find(':') {
            Some(i) => {
                let v = &line[i + 1..];
                (&line[..i], v.strip_prefix(' ').unwrap_or(v))
            }
            None => (&line[..], ""),
        };
        match field {
            "event" => self.event = value.to_string(),
            "data" => {
                if self.data.len() + value.len() >= self.limit {
                    self.data.clear();
                    self.has_data = false;
                    return Err(SseTooLarge { limit: self.limit });
                }
                self.data.push_str(value);
                self.data.push('\n');
                self.has_data = true;
            }
            "id" if !value.contains('\0') => {
                self.id = Some(value.to_string());
                self.last_id = Some(value.to_string());
            }
            "retry" => {
                self.retry = value.parse().ok();
                // Only ASCII digits count (WHATWG); anything else is ignored.
                if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
                    self.reconnect_ms = value.parse().ok().or(Some(u64::MAX));
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_split_chunks_and_line_endings() {
        let mut p = SseParser::new();
        let mut ev = p.feed(b"event: a\r\ndata: one\r").unwrap();
        assert!(ev.is_empty());
        ev.extend(p.feed(b"\ndata: two\r\n\r\n: comment\n\ndata:x\n").unwrap());
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].event, "a");
        assert_eq!(ev[0].data, "one\ntwo");
        ev = p.feed(b"\n").unwrap();
        assert_eq!(ev[0].event, "message");
        assert_eq!(ev[0].data, "x");
        assert!(p.feed(b"id: 7\nevent: e\n\n").unwrap().is_empty(), "no data, no dispatch");
        assert_eq!(p.last_event_id(), Some("7"));
        assert!(p.feed(b"data: tail").unwrap().is_empty());
        assert_eq!(p.finish().unwrap().unwrap().data, "tail");
    }

    #[test]
    fn utf8_split_across_chunks() {
        let mut p = SseParser::new();
        let text = "data: h\u{e9}llo\n\n".as_bytes();
        let (a, b) = text.split_at(8);
        assert!(p.feed(a).unwrap().is_empty());
        assert_eq!(p.feed(b).unwrap()[0].data, "h\u{e9}llo");
    }

    #[test]
    fn bom_split_across_chunks() {
        let mut p = SseParser::new();
        assert!(p.feed(&[0xEF]).unwrap().is_empty());
        assert!(p.feed(&[0xBB]).unwrap().is_empty());
        let ev = p.feed(b"\xBFdata: first\n\n").unwrap();
        assert_eq!(ev.len(), 1, "{ev:?}");
        assert_eq!(ev[0].data, "first");
        // Only a leading BOM is skipped.
        let mut p = SseParser::new();
        let ev = p.feed(b"\xEFdata: x\n\n").unwrap();
        assert!(ev.is_empty(), "{ev:?}");
    }

    #[test]
    fn endless_lines_and_events_are_refused() {
        // A line that never ends (no newline at all).
        let mut p = SseParser::with_limit(1024);
        let chunk = [b'a'; 300];
        let mut err = None;
        for _ in 0..10 {
            if let Err(e) = p.feed(&chunk) {
                err = Some(e);
                break;
            }
        }
        assert_eq!(err, Some(SseTooLarge { limit: 1024 }));
        assert_eq!(err.unwrap().to_string(), "the server sent an event larger than 1024 bytes");
        assert!(p.line.capacity() <= 2048, "{}", p.line.capacity());
        // Short data lines that never form an event (no blank line).
        let mut p = SseParser::with_limit(1024);
        let mut err = None;
        for _ in 0..100 {
            if let Err(e) = p.feed(b"data: 0123456789abcdef0123456789abcdef\n") {
                err = Some(e);
                break;
            }
        }
        assert!(err.is_some());
        assert!(p.data.is_empty());
        // Events below the limit pass, one after the other.
        let mut p = SseParser::with_limit(1024);
        for _ in 0..100 {
            let ev = p.feed(format!("data: {}\n\n", "x".repeat(900)).as_bytes()).unwrap();
            assert_eq!(ev[0].data.len(), 900);
        }
        assert_eq!(SseParser::new().limit(), MAX_EVENT_BYTES);
        assert_eq!(SseTooLarge { limit: MAX_EVENT_BYTES }.to_string(), "the server sent an event larger than 64 MiB");
    }

    #[test]
    fn retry_sets_the_reconnection_time() {
        let mut p = SseParser::new();
        assert_eq!(p.retry_ms(), None);
        // Set even by an event without data, as soon as the field is read.
        assert!(p.feed(b"id: 1\nretry: 1500\n").unwrap().is_empty());
        assert_eq!(p.retry_ms(), Some(1500));
        assert!(p.feed(b"retry: soon\nretry: -1\n\n").unwrap().is_empty());
        assert_eq!(p.retry_ms(), Some(1500), "non-digits are ignored");
        let ev = p.feed(b"retry: 20\ndata: x\n\n").unwrap();
        assert_eq!((ev[0].retry, p.retry_ms()), (Some(20), Some(20)));
        p.feed(b"retry: 99999999999999999999999\n").unwrap();
        assert_eq!(p.retry_ms(), Some(u64::MAX));
    }
}
