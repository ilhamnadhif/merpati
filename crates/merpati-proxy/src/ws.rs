//! Passive WebSocket frame parser: records messages flowing through an
//! upgraded connection without touching the bytes being relayed.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use flate2::{Decompress, FlushDecompress, Status};
use parking_lot::Mutex;

use crate::entry::{WsMessage, WsOpcode};

/// Payload bytes kept per message.
const MAX_MESSAGE_BYTES: usize = 64 * 1024;
/// Messages kept per connection (later ones are only counted).
const MAX_MESSAGES: usize = 10_000;

pub(crate) struct WsLog {
    pub messages: Vec<WsMessage>,
    pub dropped: u64,
    pub changes: u64,
    stored: usize,
    budget: usize,
}

impl WsLog {
    pub fn new(budget: usize) -> Self {
        WsLog { messages: Vec::new(), dropped: 0, changes: 0, stored: 0, budget }
    }
}

pub(crate) type SharedWsLog = Arc<Mutex<WsLog>>;

struct Frame {
    opcode: u8,
    fin: bool,
    mask: Option<[u8; 4]>,
    remaining: u64,
    mask_offset: usize,
}

struct Message {
    opcode: WsOpcode,
    compressed: bool,
    size: u64,
    data: Vec<u8>,
    cap: usize,
    truncated: bool,
    at: Duration,
}

impl Message {
    fn push(&mut self, data: &[u8]) {
        let room = self.cap.saturating_sub(self.data.len());
        if data.len() > room {
            self.truncated = true;
        }
        self.data.extend_from_slice(&data[..data.len().min(room)]);
    }
}

/// Parses one direction of a WebSocket connection.
pub(crate) struct WsSniffer {
    from_client: bool,
    log: SharedWsLog,
    started: Instant,
    header: Vec<u8>,
    frame: Option<Frame>,
    message: Option<Message>,
    control: Option<Message>,
    inflater: Option<Decompress>,
    inflate_broken: bool,
    broken: bool,
}

enum Header {
    Incomplete,
    Invalid,
    Done(Frame, bool),
}

fn parse_header(h: &[u8]) -> Header {
    if h.len() < 2 {
        return Header::Incomplete;
    }
    let fin = h[0] & 0x80 != 0;
    let rsv1 = h[0] & 0x40 != 0;
    let opcode = h[0] & 0x0f;
    let masked = h[1] & 0x80 != 0;
    let len7 = (h[1] & 0x7f) as usize;
    let ext = match len7 {
        126 => 2,
        127 => 8,
        _ => 0,
    };
    let need = 2 + ext + if masked { 4 } else { 0 };
    if h.len() < need {
        return Header::Incomplete;
    }
    if !matches!(opcode, 0 | 1 | 2 | 8 | 9 | 10) || (h[0] & 0x30) != 0 {
        return Header::Invalid;
    }
    let len = match ext {
        2 => u16::from_be_bytes([h[2], h[3]]) as u64,
        8 => u64::from_be_bytes(h[2..10].try_into().unwrap_or([0; 8])),
        _ => len7 as u64,
    };
    if opcode >= 8 && (!fin || len > 125) {
        return Header::Invalid;
    }
    let mask = masked.then(|| [h[need - 4], h[need - 3], h[need - 2], h[need - 1]]);
    Header::Done(Frame { opcode, fin, mask, remaining: len, mask_offset: 0 }, rsv1)
}

impl WsSniffer {
    pub fn new(from_client: bool, log: SharedWsLog) -> Self {
        WsSniffer {
            from_client,
            log,
            started: Instant::now(),
            header: Vec::with_capacity(14),
            frame: None,
            message: None,
            control: None,
            inflater: None,
            inflate_broken: false,
            broken: false,
        }
    }

    pub fn feed(&mut self, data: &[u8]) {
        let mut i = 0;
        while i < data.len() && !self.broken {
            let Some(frame) = self.frame.as_mut() else {
                self.header.push(data[i]);
                i += 1;
                match parse_header(&self.header) {
                    Header::Incomplete => {}
                    Header::Invalid => self.broken = true,
                    Header::Done(frame, rsv1) => {
                        self.header.clear();
                        self.start_frame(frame, rsv1);
                    }
                }
                continue;
            };
            let take = frame.remaining.min((data.len() - i) as u64) as usize;
            let raw = &data[i..i + take];
            let control = frame.opcode >= 8;
            // Only unmask what will be stored or inflated.
            let wanted = if control {
                take
            } else {
                match &self.message {
                    Some(m) if m.compressed && !self.inflate_broken => take,
                    Some(m) => take.min(m.cap.saturating_sub(m.data.len()) + 1),
                    None => 0,
                }
            };
            let mut chunk = raw[..wanted].to_vec();
            if let Some(mask) = frame.mask {
                for (j, b) in chunk.iter_mut().enumerate() {
                    *b ^= mask[(frame.mask_offset + j) % 4];
                }
            }
            frame.mask_offset = (frame.mask_offset + take) % 4;
            frame.remaining -= take as u64;
            let (done, fin) = (frame.remaining == 0, frame.fin);
            i += take;
            if control {
                if let Some(c) = self.control.as_mut() {
                    c.push(&chunk);
                }
            } else {
                self.data_payload(&chunk, take);
            }
            if done {
                self.end_frame(control, fin);
            }
        }
    }

    fn start_frame(&mut self, frame: Frame, rsv1: bool) {
        let at = self.started.elapsed();
        let len = frame.remaining;
        match frame.opcode {
            0 => {
                if self.message.is_none() {
                    self.broken = true;
                    return;
                }
            }
            1 | 2 => {
                if let Some(unfinished) = self.message.take() {
                    self.push(unfinished, true);
                }
                let cap = {
                    let log = self.log.lock();
                    MAX_MESSAGE_BYTES.min(log.budget.saturating_sub(log.stored))
                };
                self.message = Some(Message {
                    opcode: if frame.opcode == 1 { WsOpcode::Text } else { WsOpcode::Binary },
                    compressed: rsv1,
                    size: 0,
                    data: Vec::new(),
                    cap,
                    truncated: false,
                    at,
                });
            }
            op => {
                let opcode = match op {
                    8 => WsOpcode::Close,
                    9 => WsOpcode::Ping,
                    _ => WsOpcode::Pong,
                };
                self.control = Some(Message {
                    opcode,
                    compressed: false,
                    size: len,
                    data: Vec::new(),
                    cap: 125,
                    truncated: false,
                    at,
                });
            }
        }
        if let Some(m) = self.message.as_mut().filter(|_| frame.opcode < 8) {
            m.size += len;
        }
        let control = frame.opcode >= 8;
        let fin = frame.fin;
        if len == 0 {
            self.frame = None;
            self.end_frame(control, fin);
        } else {
            self.frame = Some(frame);
        }
    }

    fn data_payload(&mut self, chunk: &[u8], wire_len: usize) {
        let Some(msg) = self.message.as_mut() else { return };
        if msg.compressed && !self.inflate_broken {
            let inflater = self.inflater.get_or_insert_with(|| Decompress::new(false));
            if inflate(inflater, msg, chunk).is_err() {
                self.inflate_broken = true;
                msg.truncated = true;
            }
        } else {
            if chunk.len() < wire_len {
                msg.truncated = true;
            }
            msg.push(chunk);
        }
    }

    fn end_frame(&mut self, control: bool, fin: bool) {
        self.frame = None;
        if control {
            if let Some(c) = self.control.take() {
                self.push(c, false);
            }
            return;
        }
        if !fin {
            return;
        }
        if let Some(mut msg) = self.message.take() {
            if msg.compressed && !self.inflate_broken {
                let inflater = self.inflater.get_or_insert_with(|| Decompress::new(false));
                if inflate(inflater, &mut msg, &[0x00, 0x00, 0xff, 0xff]).is_err() {
                    self.inflate_broken = true;
                    msg.truncated = true;
                }
            }
            self.push(msg, false);
        }
    }

    fn push(&mut self, msg: Message, unfinished: bool) {
        let mut log = self.log.lock();
        log.changes += 1;
        if log.messages.len() >= MAX_MESSAGES {
            log.dropped += 1;
            return;
        }
        log.stored += msg.data.len();
        log.messages.push(WsMessage {
            from_client: self.from_client,
            opcode: msg.opcode,
            at: msg.at,
            size: msg.size,
            data: Bytes::from(msg.data),
            truncated: msg.truncated || unfinished,
            compressed: msg.compressed,
        });
    }
}

/// Inflates `input` (raw DEFLATE, permessage-deflate) into `msg`, keeping
/// the decompressor state across messages (context takeover).
fn inflate(inflater: &mut Decompress, msg: &mut Message, mut input: &[u8]) -> Result<(), ()> {
    let mut out = vec![0u8; 16 * 1024];
    loop {
        let (in_before, out_before) = (inflater.total_in(), inflater.total_out());
        let status = inflater.decompress(input, &mut out, FlushDecompress::Sync).map_err(|_| ())?;
        let consumed = (inflater.total_in() - in_before) as usize;
        let produced = (inflater.total_out() - out_before) as usize;
        msg.push(&out[..produced]);
        input = &input[consumed..];
        match status {
            Status::StreamEnd => {
                // A final block (no context takeover): start fresh next time.
                inflater.reset(false);
                return Ok(());
            }
            _ if input.is_empty() && produced < out.len() => return Ok(()),
            _ if consumed == 0 && produced == 0 => return Ok(()),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn frame(opcode: u8, fin: bool, rsv1: bool, payload: &[u8], mask: Option<[u8; 4]>) -> Vec<u8> {
        let mut out = vec![(if fin { 0x80 } else { 0 }) | (if rsv1 { 0x40 } else { 0 }) | opcode];
        let m = if mask.is_some() { 0x80 } else { 0 };
        match payload.len() {
            n if n < 126 => out.push(m | n as u8),
            n if n <= 0xffff => {
                out.push(m | 126);
                out.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                out.push(m | 127);
                out.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        match mask {
            Some(k) => {
                out.extend_from_slice(&k);
                out.extend(payload.iter().enumerate().map(|(i, b)| b ^ k[i % 4]));
            }
            None => out.extend_from_slice(payload),
        }
        out
    }

    #[test]
    fn parses_fragmented_masked_and_control_frames() {
        let log: SharedWsLog = Arc::new(Mutex::new(WsLog::new(1 << 20)));
        let mut s = WsSniffer::new(true, log.clone());
        let key = Some([1, 2, 3, 4]);
        let mut bytes = frame(1, false, false, b"hel", key);
        bytes.extend(frame(9, true, false, b"ping", key));
        bytes.extend(frame(0, true, false, b"lo", key));
        bytes.extend(frame(2, true, false, &vec![7u8; 70_000], key));
        bytes.extend(frame(8, true, false, &[0x03, 0xe8], key));
        // Feed in awkward pieces.
        for chunk in bytes.chunks(5) {
            s.feed(chunk);
        }
        let log = log.lock();
        let kinds: Vec<WsOpcode> = log.messages.iter().map(|m| m.opcode).collect();
        assert_eq!(kinds, vec![WsOpcode::Ping, WsOpcode::Text, WsOpcode::Binary, WsOpcode::Close]);
        assert_eq!(log.messages[0].data.as_ref(), b"ping");
        assert_eq!(log.messages[1].data.as_ref(), b"hello");
        assert_eq!(log.messages[1].size, 5);
        assert_eq!(log.messages[2].size, 70_000);
        assert_eq!(log.messages[2].data.len(), MAX_MESSAGE_BYTES);
        assert!(log.messages[2].truncated);
        assert!(log.messages[2].data.iter().all(|b| *b == 7));
    }

    #[test]
    fn inflates_permessage_deflate_with_context_takeover() {
        let log: SharedWsLog = Arc::new(Mutex::new(WsLog::new(1 << 20)));
        let mut s = WsSniffer::new(false, log.clone());
        let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        let mut wire = Vec::new();
        for text in ["first message first message", "second message first message"] {
            enc.write_all(text.as_bytes()).unwrap();
            enc.flush().unwrap();
            let out = std::mem::take(enc.get_mut());
            let payload = out.strip_suffix(&[0x00, 0x00, 0xff, 0xff]).unwrap_or(&out).to_vec();
            wire.extend(frame(1, true, true, &payload, None));
        }
        s.feed(&wire);
        let log = log.lock();
        assert_eq!(log.messages.len(), 2);
        assert_eq!(log.messages[0].text(), "first message first message");
        assert_eq!(log.messages[1].text(), "second message first message");
        assert!(log.messages[1].compressed);
    }

    #[test]
    fn garbage_stops_parsing() {
        let log: SharedWsLog = Arc::new(Mutex::new(WsLog::new(1 << 20)));
        let mut s = WsSniffer::new(false, log.clone());
        s.feed(&[0x8f, 0x00, 0x81, 0x01, b'a']);
        assert!(s.broken);
        assert!(log.lock().messages.is_empty());
    }
}
