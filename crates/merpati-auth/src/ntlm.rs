//! NTLM over HTTP (MS-NLMP), NTLMv2 responses only.
//!
//! The handshake is stateless on our side: the message type of the
//! `Authorization` header already on the request tells which step we are at.
//!   1. `apply` sends a NEGOTIATE (type 1) message.
//!   2. Server answers 401 `WWW-Authenticate: NTLM <CHALLENGE>`.
//!   3. `handle_challenge` computes AUTHENTICATE (type 3) and asks for a retry.
//!      The NEGOTIATE bytes covered by the MIC are read back from the request.
//!
//! Over HTTPS the AUTHENTICATE message is bound to the TLS channel (Extended
//! Protection): MsvAvChannelBindings is the MD5 of a `gss_channel_bindings_struct`
//! carrying the `tls-server-end-point` binding of the server certificate the
//! CHALLENGE arrived with (see [`crate::channel_binding`]).
//!
//! AUTHENTICATE follows MS-NLMP 3.1.5.1.2 and matches pyspnego byte for byte,
//! apart from two documented cases checked with pyspnego's acceptor instead
//! (vectors and notes in `tests/fixtures/ntlm`):
//! - the target info sent back is the server's plus MsvAvChannelBindings (all
//!   zero over plain HTTP, when no TLS channel binding is known) and
//!   MsvAvTargetName (`HTTP/host[:port]`);
//! - when the CHALLENGE carries MsvAvTimestamp, that timestamp is the NTLMv2
//!   client time, MsvAvFlags gets the MIC-present bit, LmChallengeResponse is
//!   Z(24) and the MIC is HMAC_MD5(ExportedSessionKey, NEGOTIATE || CHALLENGE ||
//!   AUTHENTICATE with the MIC zeroed);
//! - with NTLMSSP_NEGOTIATE_KEY_EXCH and SIGN or SEAL, ExportedSessionKey is a
//!   random key sent as RC4K(KeyExchangeKey, key); otherwise it is the
//!   KeyExchangeKey (= NTLMv2 SessionBaseKey), which is what servers derive
//!   (MS-NLMP 3.2.5.1.2).

use base64::Engine;
use hmac::{Hmac, Mac};
use md5::{Digest, Md5};
use rand::Rng;

use merpati_core::PreparedRequest;

use crate::challenge::parse_challenges;
use crate::error::AuthError;
use crate::util::B64;

const SIGNATURE: &[u8; 8] = b"NTLMSSP\0";
const NEGOTIATE_UNICODE: u32 = 0x0000_0001;
const NEGOTIATE_OEM: u32 = 0x0000_0002;
const REQUEST_TARGET: u32 = 0x0000_0004;
const NEGOTIATE_SIGN: u32 = 0x0000_0010;
const NEGOTIATE_SEAL: u32 = 0x0000_0020;
const NEGOTIATE_NTLM: u32 = 0x0000_0200;
const NEGOTIATE_ALWAYS_SIGN: u32 = 0x0000_8000;
const NEGOTIATE_EXTENDED_SESSIONSECURITY: u32 = 0x0008_0000;
#[cfg(test)]
const NEGOTIATE_TARGET_INFO: u32 = 0x0080_0000;
const NEGOTIATE_VERSION: u32 = 0x0200_0000;
const NEGOTIATE_128: u32 = 0x2000_0000;
const NEGOTIATE_KEY_EXCH: u32 = 0x4000_0000;
const NEGOTIATE_56: u32 = 0x8000_0000;
const AV_EOL: u16 = 0;
const AV_FLAGS: u16 = 6;
const AV_TIMESTAMP: u16 = 7;
const AV_TARGET_NAME: u16 = 9;
const AV_CHANNEL_BINDINGS: u16 = 10;
/// MsvAvFlags bit: the AUTHENTICATE message carries a MIC.
const AV_FLAG_MIC: u32 = 0x0000_0002;
/// VERSION (MS-NLMP 2.2.2.10) sent when the server negotiates
/// NTLMSSP_NEGOTIATE_VERSION: 10.0 build 19041, NTLMSSP_REVISION_W2K3. It is
/// informational only; all zero otherwise.
const VERSION: [u8; 8] = [10, 0, 0x61, 0x4a, 0, 0, 0, 0x0f];
/// AUTHENTICATE fixed part: 6 security buffers and flags (64), Version (8), MIC (16).
const AUTH_HEADER_LEN: usize = 88;
const MIC_OFFSET: usize = 72;

// ---------------------------------------------------------------------------
// MD4 (RFC 1320). Only used for the NT hash; implemented here to avoid a
// dependency for 60 lines of code.
// ---------------------------------------------------------------------------

pub(crate) fn md4(input: &[u8]) -> [u8; 16] {
    let mut msg = input.to_vec();
    let bit_len = (input.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_le_bytes());

    let (mut a, mut b, mut c, mut d) = (0x6745_2301u32, 0xefcd_ab89u32, 0x98ba_dcfeu32, 0x1032_5476u32);
    let f = |x: u32, y: u32, z: u32| (x & y) | (!x & z);
    let g = |x: u32, y: u32, z: u32| (x & y) | (x & z) | (y & z);
    let h = |x: u32, y: u32, z: u32| x ^ y ^ z;
    for chunk in msg.chunks(64) {
        let x: Vec<u32> = chunk.chunks(4).map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]])).collect();
        let (aa, bb, cc, dd) = (a, b, c, d);
        for &i in &[0usize, 4, 8, 12] {
            a = a.wrapping_add(f(b, c, d)).wrapping_add(x[i]).rotate_left(3);
            d = d.wrapping_add(f(a, b, c)).wrapping_add(x[i + 1]).rotate_left(7);
            c = c.wrapping_add(f(d, a, b)).wrapping_add(x[i + 2]).rotate_left(11);
            b = b.wrapping_add(f(c, d, a)).wrapping_add(x[i + 3]).rotate_left(19);
        }
        for &i in &[0usize, 1, 2, 3] {
            a = a.wrapping_add(g(b, c, d)).wrapping_add(x[i]).wrapping_add(0x5a82_7999).rotate_left(3);
            d = d.wrapping_add(g(a, b, c)).wrapping_add(x[i + 4]).wrapping_add(0x5a82_7999).rotate_left(5);
            c = c.wrapping_add(g(d, a, b)).wrapping_add(x[i + 8]).wrapping_add(0x5a82_7999).rotate_left(9);
            b = b.wrapping_add(g(c, d, a)).wrapping_add(x[i + 12]).wrapping_add(0x5a82_7999).rotate_left(13);
        }
        for &i in &[0usize, 2, 1, 3] {
            a = a.wrapping_add(h(b, c, d)).wrapping_add(x[i]).wrapping_add(0x6ed9_eba1).rotate_left(3);
            d = d.wrapping_add(h(a, b, c)).wrapping_add(x[i + 8]).wrapping_add(0x6ed9_eba1).rotate_left(9);
            c = c.wrapping_add(h(d, a, b)).wrapping_add(x[i + 4]).wrapping_add(0x6ed9_eba1).rotate_left(11);
            b = b.wrapping_add(h(c, d, a)).wrapping_add(x[i + 12]).wrapping_add(0x6ed9_eba1).rotate_left(15);
        }
        a = a.wrapping_add(aa);
        b = b.wrapping_add(bb);
        c = c.wrapping_add(cc);
        d = d.wrapping_add(dd);
    }
    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&a.to_le_bytes());
    out[4..8].copy_from_slice(&b.to_le_bytes());
    out[8..12].copy_from_slice(&c.to_le_bytes());
    out[12..16].copy_from_slice(&d.to_le_bytes());
    out
}

fn hmac_md5(key: &[u8], parts: &[&[u8]]) -> [u8; 16] {
    let mut m = Hmac::<Md5>::new_from_slice(key).expect("any key length");
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().into()
}

/// RC4 (only used as RC4K for EncryptedRandomSessionKey).
fn rc4(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut s: [u8; 256] = std::array::from_fn(|i| i as u8);
    let mut j = 0u8;
    for i in 0..256 {
        j = j.wrapping_add(s[i]).wrapping_add(key[i % key.len()]);
        s.swap(i, j as usize);
    }
    let (mut i, mut j) = (0u8, 0u8);
    data.iter()
        .map(|b| {
            i = i.wrapping_add(1);
            j = j.wrapping_add(s[i as usize]);
            s.swap(i as usize, j as usize);
            b ^ s[s[i as usize].wrapping_add(s[j as usize]) as usize]
        })
        .collect()
}

fn utf16le(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

pub(crate) fn nt_hash(password: &str) -> [u8; 16] {
    md4(&utf16le(password))
}

/// NTOWFv2 = HMAC_MD5(MD4(UNICODE(password)), UNICODE(Uppercase(user) || domain)).
pub(crate) fn ntowf_v2(user: &str, password: &str, domain: &str) -> [u8; 16] {
    let identity = utf16le(&format!("{}{}", user.to_uppercase(), domain));
    hmac_md5(&nt_hash(password), &[&identity])
}

/// Hash for MsvAvChannelBindings: MD5 of a `gss_channel_bindings_struct`
/// without addresses whose application data is e.g. `tls-server-end-point:`
/// followed by the certificate hash (RFC 5929). All zero when unknown.
pub(crate) fn channel_bindings_hash(application_data: Option<&[u8]>) -> [u8; 16] {
    let Some(data) = application_data else { return [0; 16] };
    let mut m = Md5::new();
    // initiator addrtype/length, acceptor addrtype/length.
    m.update([0u8; 16]);
    m.update((data.len() as u32).to_le_bytes());
    m.update(data);
    m.finalize().into()
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

pub(crate) fn negotiate_message() -> Vec<u8> {
    let flags = NEGOTIATE_UNICODE
        | NEGOTIATE_OEM
        | REQUEST_TARGET
        | NEGOTIATE_NTLM
        | NEGOTIATE_ALWAYS_SIGN
        | NEGOTIATE_EXTENDED_SESSIONSECURITY
        | NEGOTIATE_128
        | NEGOTIATE_56;
    let mut m = Vec::with_capacity(32);
    m.extend_from_slice(SIGNATURE);
    m.extend_from_slice(&1u32.to_le_bytes());
    m.extend_from_slice(&flags.to_le_bytes());
    // Empty domain and workstation security buffers (offset 32).
    for _ in 0..2 {
        m.extend_from_slice(&0u16.to_le_bytes());
        m.extend_from_slice(&0u16.to_le_bytes());
        m.extend_from_slice(&32u32.to_le_bytes());
    }
    m
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChallengeMessage {
    pub flags: u32,
    pub server_challenge: [u8; 8],
    pub target_info: Vec<u8>,
    /// The message as received; part of the MIC.
    pub raw: Vec<u8>,
}

fn read_u16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*b.get(at)?, *b.get(at + 1)?]))
}

fn read_u32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes([*b.get(at)?, *b.get(at + 1)?, *b.get(at + 2)?, *b.get(at + 3)?]))
}

pub(crate) fn message_type(b: &[u8]) -> Option<u32> {
    if b.len() < 12 || &b[..8] != SIGNATURE {
        return None;
    }
    read_u32(b, 8)
}

pub(crate) fn parse_challenge(b: &[u8]) -> Result<ChallengeMessage, AuthError> {
    let bad = || AuthError::invalid("malformed NTLM challenge from server");
    if message_type(b) != Some(2) || b.len() < 32 {
        return Err(bad());
    }
    let flags = read_u32(b, 20).ok_or_else(bad)?;
    let mut server_challenge = [0u8; 8];
    server_challenge.copy_from_slice(&b[24..32]);
    let mut target_info = Vec::new();
    if b.len() >= 48 {
        let len = read_u16(b, 40).ok_or_else(bad)? as usize;
        let off = read_u32(b, 44).ok_or_else(bad)? as usize;
        if len > 0 {
            target_info = b.get(off..off.checked_add(len).ok_or_else(bad)?).ok_or_else(bad)?.to_vec();
        }
    }
    Ok(ChallengeMessage { flags, server_challenge, target_info, raw: b.to_vec() })
}

/// AV_PAIRs of a target info block, up to MsvAvEOL.
fn av_pairs(target_info: &[u8]) -> Vec<(u16, &[u8])> {
    let mut out = Vec::new();
    let mut i = 0;
    while let (Some(id), Some(len)) = (read_u16(target_info, i), read_u16(target_info, i + 2)) {
        let Some(value) = target_info.get(i + 4..i + 4 + len as usize) else { break };
        if id == AV_EOL {
            break;
        }
        out.push((id, value));
        i += 4 + len as usize;
    }
    out
}

fn av_get(target_info: &[u8], id: u16) -> Option<&[u8]> {
    av_pairs(target_info).into_iter().find(|(k, _)| *k == id).map(|(_, v)| v)
}

/// MsvAvTimestamp from the target info, if present.
fn av_timestamp(target_info: &[u8]) -> Option<[u8; 8]> {
    av_get(target_info, AV_TIMESTAMP).and_then(|v| v.try_into().ok())
}

/// The target info the client returns inside the NTLMv2 response: the
/// server's pairs with MsvAvChannelBindings, MsvAvTargetName and (when a MIC
/// is sent) MsvAvFlags set in place or appended, in that order.
fn client_target_info(server: &[u8], channel_bindings: [u8; 16], target_name: &str, mic: bool) -> Vec<u8> {
    let mut pairs: Vec<(u16, Vec<u8>)> = av_pairs(server).into_iter().map(|(k, v)| (k, v.to_vec())).collect();
    let mut set = |id: u16, value: Vec<u8>| match pairs.iter_mut().find(|(k, _)| *k == id) {
        Some(pair) => pair.1 = value,
        None => pairs.push((id, value)),
    };
    set(AV_CHANNEL_BINDINGS, channel_bindings.to_vec());
    set(AV_TARGET_NAME, utf16le(target_name));
    if mic {
        let flags = av_get(server, AV_FLAGS).and_then(|v| v.try_into().ok()).map(u32::from_le_bytes).unwrap_or(0);
        set(AV_FLAGS, (flags | AV_FLAG_MIC).to_le_bytes().to_vec());
    }
    let mut out = Vec::new();
    for (id, value) in pairs.iter().chain([(AV_EOL, Vec::new())].iter()) {
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(&(value.len() as u16).to_le_bytes());
        out.extend_from_slice(value);
    }
    out
}

/// Windows FILETIME: 100ns intervals since 1601-01-01.
fn filetime_now() -> [u8; 8] {
    let unix = chrono::Utc::now();
    let ticks = (unix.timestamp() as u64 + 11_644_473_600) * 10_000_000 + (unix.timestamp_subsec_nanos() as u64 / 100);
    ticks.to_le_bytes()
}

pub(crate) struct V2Response {
    pub nt: Vec<u8>,
    /// LMv2 response (replaced by Z(24) when a MIC is sent).
    pub lm: Vec<u8>,
    /// SessionBaseKey, which is also the KeyExchangeKey for NTLMv2.
    pub session_base_key: [u8; 16],
}

/// NTLMv2 NT and LM challenge responses (MS-NLMP 3.3.2). `av_pairs` is the
/// target info embedded in the response (`ServerName`).
pub(crate) fn ntlmv2_response(
    response_key: &[u8; 16],
    server_challenge: &[u8; 8],
    client_challenge: [u8; 8],
    time: [u8; 8],
    av_pairs: &[u8],
) -> V2Response {
    let mut temp = vec![0x01, 0x01, 0, 0, 0, 0, 0, 0];
    temp.extend_from_slice(&time);
    temp.extend_from_slice(&client_challenge);
    temp.extend_from_slice(&[0, 0, 0, 0]);
    temp.extend_from_slice(av_pairs);
    temp.extend_from_slice(&[0, 0, 0, 0]);
    let proof = hmac_md5(response_key, &[server_challenge, &temp]);
    let session_base_key = hmac_md5(response_key, &[&proof]);
    let mut nt = proof.to_vec();
    nt.extend_from_slice(&temp);
    let mut lm = hmac_md5(response_key, &[server_challenge, &client_challenge]).to_vec();
    lm.extend_from_slice(&client_challenge);
    V2Response { nt, lm, session_base_key }
}

/// Who authenticates, and to which service.
pub(crate) struct ClientInput<'a> {
    pub user: &'a str,
    pub password: &'a str,
    pub domain: &'a str,
    pub workstation: &'a str,
    /// MsvAvTargetName: the server's SPN, `HTTP/host[:port]`.
    pub target_name: &'a str,
    /// Unhashed channel bindings (see [`channel_bindings_hash`]), when known.
    pub channel_bindings: Option<&'a [u8]>,
}

/// Random inputs, fixed in tests.
pub(crate) struct Nonces {
    pub client_challenge: [u8; 8],
    /// Client time, used only when the server sends no MsvAvTimestamp.
    pub now: [u8; 8],
    /// ExportedSessionKey when KEY_EXCH is negotiated with SIGN or SEAL.
    pub session_key: [u8; 16],
}

impl Nonces {
    fn random() -> Self {
        let mut rng = rand::rng();
        Nonces { client_challenge: rng.random(), now: filetime_now(), session_key: rng.random() }
    }
}

pub(crate) struct Authenticate {
    pub message: Vec<u8>,
    /// Key of the MIC (and of signing/sealing, which HTTP does not use).
    #[cfg_attr(not(test), expect(dead_code, reason = "checked against the reference vectors"))]
    pub exported_session_key: [u8; 16],
}

/// Builds the AUTHENTICATE message answering `ch`, sent after `negotiate`.
pub(crate) fn authenticate_message(
    c: &ClientInput<'_>,
    negotiate: &[u8],
    ch: &ChallengeMessage,
    n: &Nonces,
) -> Authenticate {
    let server_time = av_timestamp(&ch.target_info);
    let mic = server_time.is_some();
    let target_info =
        client_target_info(&ch.target_info, channel_bindings_hash(c.channel_bindings), c.target_name, mic);
    let key = ntowf_v2(c.user, c.password, c.domain);
    let r = ntlmv2_response(&key, &ch.server_challenge, n.client_challenge, server_time.unwrap_or(n.now), &target_info);
    // MS-NLMP 3.1.5.1.2: with a server timestamp the LM response is Z(24).
    let lm = if mic { vec![0u8; 24] } else { r.lm };

    let flags = ch.flags;
    let key_exchange_key = r.session_base_key;
    let (exported_session_key, encrypted_key) = if flags & NEGOTIATE_KEY_EXCH == 0 {
        (key_exchange_key, Vec::new())
    } else if flags & (NEGOTIATE_SIGN | NEGOTIATE_SEAL) != 0 {
        (n.session_key, rc4(&key_exchange_key, &n.session_key))
    } else {
        // Servers only decrypt the key when signing or sealing; without, they
        // use the KeyExchangeKey. Sending it encrypted with itself keeps the
        // field the expected 16 bytes and works with either reading.
        (key_exchange_key, rc4(&key_exchange_key, &key_exchange_key))
    };

    let unicode = flags & NEGOTIATE_UNICODE != 0;
    let enc = |s: &str| if unicode { utf16le(s) } else { s.as_bytes().to_vec() };
    let fields: [Vec<u8>; 6] = [lm, r.nt, enc(c.domain), enc(c.user), enc(c.workstation), encrypted_key];
    let mut m = Vec::with_capacity(AUTH_HEADER_LEN + fields.iter().map(Vec::len).sum::<usize>());
    m.extend_from_slice(SIGNATURE);
    m.extend_from_slice(&3u32.to_le_bytes());
    let mut offset = AUTH_HEADER_LEN as u32;
    for f in &fields {
        m.extend_from_slice(&(f.len() as u16).to_le_bytes());
        m.extend_from_slice(&(f.len() as u16).to_le_bytes());
        m.extend_from_slice(&offset.to_le_bytes());
        offset += f.len() as u32;
    }
    m.extend_from_slice(&flags.to_le_bytes());
    m.extend_from_slice(if flags & NEGOTIATE_VERSION != 0 { &VERSION } else { &[0; 8] });
    m.extend_from_slice(&[0; 16]);
    debug_assert_eq!(m.len(), AUTH_HEADER_LEN);
    for f in &fields {
        m.extend_from_slice(f);
    }
    if mic {
        let mic = hmac_md5(&exported_session_key, &[negotiate, &ch.raw, &m]);
        m[MIC_OFFSET..MIC_OFFSET + 16].copy_from_slice(&mic);
    }
    Authenticate { message: m, exported_session_key }
}

/// The NTLM message currently in the `Authorization` header.
fn sent_message(req: &PreparedRequest) -> Option<Vec<u8>> {
    let h = req.header("authorization")?;
    let token = h.strip_prefix("NTLM ").or_else(|| h.strip_prefix("Negotiate "))?;
    B64.decode(token.trim()).ok().filter(|b| message_type(b).is_some())
}

/// `(user, domain)`: the domain field, or else parsed from `DOMAIN\user` or
/// `user@DOMAIN` like Postman.
fn split_user<'a>(user: &'a str, domain: &'a str) -> (&'a str, &'a str) {
    if !domain.is_empty() || (user.contains('\\') && user.contains('@')) {
        return (user, domain);
    }
    for sep in ['\\', '@'] {
        let parts: Vec<&str> = user.split(sep).collect();
        if let [a, b] = parts[..]
            && !a.is_empty()
            && !b.is_empty()
        {
            return if sep == '\\' { (b, a) } else { (a, b) };
        }
    }
    (user, domain)
}

/// SPN for MsvAvTargetName: `HTTP/host`, plus the port when not the default.
fn target_name(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(u) => format!("HTTP/{}", crate::util::host_header(&u)),
        Err(_) => String::new(),
    }
}

/// AUTHENTICATE header value answering the CHALLENGE `token`, bound to
/// `channel_bindings` (unhashed application data) when given.
fn answer_challenge(
    req: &PreparedRequest,
    negotiate: &[u8],
    token: &str,
    channel_bindings: Option<&[u8]>,
    n: &Nonces,
) -> Result<String, AuthError> {
    let bytes = B64.decode(token.trim()).map_err(|_| AuthError::invalid("malformed NTLM challenge from server"))?;
    let ch = parse_challenge(&bytes)?;
    let auth = &req.auth;
    let (user, domain) = split_user(auth.get("username"), auth.get("domain"));
    let workstation = match auth.get("workstation") {
        "" => "MERPATI",
        w => w,
    };
    let spn = target_name(&req.url);
    let input =
        ClientInput { user, password: auth.get("password"), domain, workstation, target_name: &spn, channel_bindings };
    let msg = authenticate_message(&input, negotiate, &ch, n);
    Ok(format!("NTLM {}", B64.encode(msg.message)))
}

pub(crate) fn apply(req: &mut PreparedRequest) -> Result<(), AuthError> {
    if req.auth.get("username").is_empty() {
        return Err(AuthError::Missing { scheme: "NTLM", field: "username" });
    }
    req.set_header("Authorization", format!("NTLM {}", B64.encode(negotiate_message())));
    Ok(())
}

/// `peer_certificate`: DER of the server's TLS leaf certificate, `None` over
/// plain HTTP.
pub(crate) fn handle_challenge(
    req: &mut PreparedRequest,
    headers: &[(String, String)],
    peer_certificate: Option<&[u8]>,
) -> Result<bool, AuthError> {
    if req.auth.get_bool("disableRetryRequest", false) {
        return Ok(false);
    }
    let values = headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case("www-authenticate")).map(|(_, v)| v.as_str());
    let challenges = parse_challenges(values);
    let Some(ntlm) = challenges.iter().find(|c| c.scheme == "ntlm") else { return Ok(false) };
    let sent = sent_message(req);
    match (&ntlm.token68, sent.as_deref().and_then(message_type)) {
        // Server wants NTLM but we have not started: send NEGOTIATE.
        (None, None) => {
            req.set_header("Authorization", format!("NTLM {}", B64.encode(negotiate_message())));
            Ok(true)
        }
        // CHALLENGE received in response to our NEGOTIATE.
        (Some(token), Some(1)) => {
            let negotiate = sent.unwrap_or_default();
            let bindings = peer_certificate.map(crate::channel_binding::tls_server_end_point);
            let value = answer_challenge(req, &negotiate, token, bindings.as_deref(), &Nonces::random())?;
            req.set_header("Authorization", value);
            Ok(true)
        }
        // Anything else (rejected AUTHENTICATE, unexpected challenge): give up.
        _ => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use merpati_core::{Auth, AuthType};
    use serde_json::Value;

    fn sent_message_type(req: &PreparedRequest) -> Option<u32> {
        sent_message(req).and_then(|b| message_type(&b))
    }

    #[test]
    fn md4_rfc1320() {
        assert_eq!(hex::encode(md4(b"")), "31d6cfe0d16ae931b73c59d7e0c089c0");
        assert_eq!(hex::encode(md4(b"abc")), "a448017aaf21d8525fc10ae87aa6729d");
        assert_eq!(hex::encode(md4(b"message digest")), "d9130a8164549fe818874806e1c7014b");
        assert_eq!(
            hex::encode(md4(b"12345678901234567890123456789012345678901234567890123456789012345678901234567890")),
            "e33b4ddc9c38f2199c3e7b164fcc0536"
        );
    }

    #[test]
    fn rc4_known_answer() {
        // Classic test vectors (key "Key"/"Wiki"/"Secret").
        assert_eq!(hex::encode(rc4(b"Key", b"Plaintext")), "bbf316e8d940af0ad3");
        assert_eq!(hex::encode(rc4(b"Wiki", b"pedia")), "1021bf0420");
        assert_eq!(hex::encode(rc4(b"Secret", b"Attack at dawn")), "45a01f645fc35b383552544b9bf5");
    }

    fn av(id: u16, value: &[u8]) -> Vec<u8> {
        let mut v = id.to_le_bytes().to_vec();
        v.extend_from_slice(&(value.len() as u16).to_le_bytes());
        v.extend_from_slice(value);
        v
    }

    #[test]
    fn ms_nlmp_ntlmv2_vectors() {
        // MS-NLMP 4.2.1 / 4.2.4.
        assert_eq!(hex::encode(nt_hash("Password")), "a4f49c406510bdcab6824ee7c30fd852");
        let key = ntowf_v2("User", "Password", "Domain");
        assert_eq!(hex::encode(key), "0c868a403bfd7a93a3001ef22ef02e3f");
        let mut target_info = av(2, &utf16le("Domain"));
        target_info.extend(av(1, &utf16le("Server")));
        target_info.extend(av(0, &[]));
        let server_challenge = [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef];
        let r = ntlmv2_response(&key, &server_challenge, [0xaa; 8], [0; 8], &target_info);
        assert_eq!(hex::encode(&r.nt[..16]), "68cd0ab851e51c96aabc927bebef6a1c");
        assert_eq!(hex::encode(&r.lm), "86c35097ac9cec102554764a57cccc19aaaaaaaaaaaaaaaa");
        // 4.2.4.1.2 session base key, 4.2.4.2.3 EncryptedRandomSessionKey (RandomSessionKey = 0x55 * 16).
        assert_eq!(hex::encode(r.session_base_key), "8de40ccadbc14a82f15cb0ad0de95ca3");
        assert_eq!(hex::encode(rc4(&r.session_base_key, &[0x55; 16])), "c5dad2544fc9799094ce1ce90bc9d03e");
    }

    fn bytes(v: &Value, key: &str) -> Vec<u8> {
        hex::decode(v[key].as_str().unwrap_or_else(|| panic!("{key}"))).unwrap()
    }

    fn array<const N: usize>(v: &Value, key: &str) -> [u8; N] {
        bytes(v, key).try_into().unwrap()
    }

    /// AUTHENTICATE messages produced by pyspnego 0.12.4 (see the fixture's README).
    #[test]
    fn matches_pyspnego_byte_for_byte() {
        let doc: Value = serde_json::from_str(include_str!("../tests/fixtures/ntlm/pyspnego_vectors.json")).unwrap();
        let vectors = doc["vectors"].as_array().unwrap();
        assert!(vectors.len() >= 6);
        for v in vectors {
            let name = v["name"].as_str().unwrap();
            let s = |k: &str| v[k].as_str().unwrap().to_string();
            let cb = v["channel_bindings"].as_str().map(|h| hex::decode(h).unwrap());
            let (user, password, domain, workstation, spn) =
                (s("user"), s("password"), s("domain"), s("workstation"), s("target_name"));
            let input = ClientInput {
                user: &user,
                password: &password,
                domain: &domain,
                workstation: &workstation,
                target_name: &spn,
                channel_bindings: cb.as_deref(),
            };
            let nonces = Nonces {
                client_challenge: array(v, "client_challenge"),
                now: array(v, "now"),
                session_key: array(v, "session_key"),
            };
            let negotiate = bytes(v, "negotiate");
            let ch = parse_challenge(&bytes(v, "challenge")).unwrap();
            let got = authenticate_message(&input, &negotiate, &ch, &nonces);
            assert_eq!(hex::encode(&got.message), s("authenticate"), "{name}: AUTHENTICATE");
            assert_eq!(hex::encode(got.exported_session_key), s("exported_session_key"), "{name}: session key");
        }
        // Our NEGOTIATE is what the vectors' MICs cover.
        assert_eq!(hex::encode(negotiate_message()), vectors[0]["negotiate"].as_str().unwrap());
    }

    /// Messages pyspnego's own acceptor (server side) verified: NT proof, MIC
    /// and channel bindings, including the KEY_EXCH-without-signing case
    /// where we deliberately differ from pyspnego's initiator.
    #[test]
    fn accepted_by_pyspnego_acceptor() {
        let doc: Value = serde_json::from_str(include_str!("../tests/fixtures/ntlm/pyspnego_vectors.json")).unwrap();
        for v in doc["accepted"].as_array().unwrap() {
            let name = v["name"].as_str().unwrap();
            let s = |k: &str| v[k].as_str().unwrap().to_string();
            let (user, password, domain, workstation, spn) =
                (s("user"), s("password"), s("domain"), s("workstation"), s("target_name"));
            let input = ClientInput {
                user: &user,
                password: &password,
                domain: &domain,
                workstation: &workstation,
                target_name: &spn,
                channel_bindings: None,
            };
            let nonces = Nonces {
                client_challenge: array(v, "client_challenge"),
                now: array(v, "now"),
                session_key: array(v, "session_key"),
            };
            let ch = parse_challenge(&bytes(v, "challenge")).unwrap();
            let got = authenticate_message(&input, &bytes(v, "negotiate"), &ch, &nonces);
            assert_eq!(hex::encode(&got.message), s("authenticate"), "{name}");
            assert_eq!(hex::encode(got.exported_session_key), s("exported_session_key"), "{name}: session key");
        }
    }

    #[test]
    fn channel_bindings_hash_matches_gss_struct() {
        assert_eq!(channel_bindings_hash(None), [0; 16]);
        // pyspnego: md5(GssChannelBindings().pack()), i.e. 20 zero bytes.
        assert_eq!(hex::encode(channel_bindings_hash(Some(b""))), "441018525208457705bf09a8ee3c1093");
    }

    fn challenge_bytes(flags: u32, target_info: &[u8]) -> Vec<u8> {
        let mut m = SIGNATURE.to_vec();
        m.extend_from_slice(&2u32.to_le_bytes());
        let name = utf16le("Server");
        m.extend_from_slice(&(name.len() as u16).to_le_bytes());
        m.extend_from_slice(&(name.len() as u16).to_le_bytes());
        m.extend_from_slice(&48u32.to_le_bytes());
        m.extend_from_slice(&flags.to_le_bytes());
        m.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        m.extend_from_slice(&[0; 8]);
        m.extend_from_slice(&(target_info.len() as u16).to_le_bytes());
        m.extend_from_slice(&(target_info.len() as u16).to_le_bytes());
        m.extend_from_slice(&((48 + name.len()) as u32).to_le_bytes());
        m.extend_from_slice(&name);
        m.extend_from_slice(target_info);
        m
    }

    fn field(msg: &[u8], at: usize) -> &[u8] {
        let len = read_u16(msg, at).unwrap() as usize;
        let off = read_u32(msg, at + 4).unwrap() as usize;
        &msg[off..off + len]
    }

    #[test]
    fn handshake_steps() {
        let mut req = PreparedRequest {
            url: "http://intranet:8080/".into(),
            auth: Auth::new(AuthType::Ntlm)
                .with("username", "User")
                .with("password", "Password")
                .with("domain", "Domain"),
            ..Default::default()
        };
        apply(&mut req).unwrap();
        assert_eq!(sent_message_type(&req), Some(1));
        let negotiate = sent_message(&req).unwrap();

        let mut ti = av(2, &utf16le("Domain"));
        ti.extend(av(AV_TIMESTAMP, &[9; 8]));
        ti.extend(av(0, &[]));
        let raw = challenge_bytes(NEGOTIATE_UNICODE | NEGOTIATE_NTLM | NEGOTIATE_TARGET_INFO, &ti);
        let parsed = parse_challenge(&raw).unwrap();
        assert_eq!(parsed.server_challenge, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(av_timestamp(&parsed.target_info), Some([9; 8]));

        let headers = vec![("WWW-Authenticate".to_string(), format!("NTLM {}", B64.encode(&raw)))];
        assert!(handle_challenge(&mut req, &headers, None).unwrap());
        assert_eq!(sent_message_type(&req), Some(3));
        let msg = sent_message(&req).unwrap();
        // LM response is zeroed because the server sent a timestamp.
        assert_eq!(field(&msg, 12), &[0u8; 24]);
        // User name is UTF-16LE.
        assert_eq!(field(&msg, 36), utf16le("User").as_slice());
        // The NTLMv2 blob reuses the server timestamp and announces the MIC.
        let nt = field(&msg, 20);
        assert_eq!(&nt[24..32], &[9; 8]);
        let blob_ti = &nt[44..nt.len() - 4];
        assert_eq!(av_get(blob_ti, AV_FLAGS), Some(&AV_FLAG_MIC.to_le_bytes()[..]));
        assert_eq!(av_get(blob_ti, AV_CHANNEL_BINDINGS), Some(&[0u8; 16][..]));
        assert_eq!(av_get(blob_ti, AV_TARGET_NAME), Some(utf16le("HTTP/intranet:8080").as_slice()));
        // MIC = HMAC_MD5(SessionBaseKey, NEGOTIATE || CHALLENGE || AUTHENTICATE with zeroed MIC).
        let key = ntowf_v2("User", "Password", "Domain");
        let base_key = hmac_md5(&key, &[&nt[..16]]);
        let mut zeroed = msg.clone();
        zeroed[MIC_OFFSET..MIC_OFFSET + 16].fill(0);
        assert_eq!(&msg[MIC_OFFSET..MIC_OFFSET + 16], &hmac_md5(&base_key, &[&negotiate, &raw, &zeroed]));

        // A further challenge after AUTHENTICATE means bad credentials.
        assert!(!handle_challenge(&mut req, &headers, None).unwrap());
    }

    #[test]
    fn https_binds_the_server_certificate() {
        use rustls::pki_types::pem::PemObject;
        use sha2::Sha512;
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tls_binding/rsa_sha512.pem");
        let cert = rustls::pki_types::CertificateDer::from_pem_file(path).unwrap();
        let mut req = PreparedRequest {
            url: "https://intranet/".into(),
            auth: Auth::new(AuthType::Ntlm).with("username", "User").with("password", "Password"),
            ..Default::default()
        };
        apply(&mut req).unwrap();
        let raw = challenge_bytes(NEGOTIATE_UNICODE | NEGOTIATE_NTLM, &av(0, &[]));
        let headers = vec![("WWW-Authenticate".to_string(), format!("NTLM {}", B64.encode(&raw)))];
        assert!(handle_challenge(&mut req, &headers, Some(&cert)).unwrap());
        let nt = field(&sent_message(&req).unwrap(), 20).to_vec();
        // MD5(gss_channel_bindings_struct): zero addresses, then the
        // length-prefixed "tls-server-end-point:" || SHA-512(certificate).
        let app_data = [b"tls-server-end-point:".as_slice(), &Sha512::digest(&cert)].concat();
        let mut gss = vec![0u8; 16];
        gss.extend_from_slice(&(app_data.len() as u32).to_le_bytes());
        gss.extend_from_slice(&app_data);
        let expected: [u8; 16] = Md5::digest(&gss).into();
        assert_eq!(av_get(&nt[44..nt.len() - 4], AV_CHANNEL_BINDINGS), Some(&expected[..]));
    }

    #[test]
    fn no_server_timestamp_means_no_mic_and_lmv2() {
        let ti = [av(2, &utf16le("Domain")), av(0, &[])].concat();
        let ch = parse_challenge(&challenge_bytes(NEGOTIATE_UNICODE | NEGOTIATE_NTLM, &ti)).unwrap();
        let input = ClientInput {
            user: "User",
            password: "Password",
            domain: "Domain",
            workstation: "WS",
            target_name: "HTTP/h",
            channel_bindings: None,
        };
        let n = Nonces { client_challenge: [0xaa; 8], now: [7; 8], session_key: [0x55; 16] };
        let a = authenticate_message(&input, &negotiate_message(), &ch, &n);
        assert_eq!(&a.message[MIC_OFFSET..MIC_OFFSET + 16], &[0; 16]);
        let lm = field(&a.message, 12);
        assert_eq!(lm.len(), 24);
        assert_eq!(&lm[16..], &[0xaa; 8]);
        let nt = field(&a.message, 20);
        assert_eq!(&nt[24..32], &[7; 8]);
        assert_eq!(av_get(&nt[44..nt.len() - 4], AV_FLAGS), None);
        // Without KEY_EXCH the exported key is the session base key and no key is sent.
        assert_eq!(a.exported_session_key, hmac_md5(&ntowf_v2("User", "Password", "Domain"), &[&nt[..16]]));
        assert!(field(&a.message, 52).is_empty());
    }

    #[test]
    fn user_and_domain_parsing() {
        assert_eq!(split_user("CORP\\alice", ""), ("alice", "CORP"));
        assert_eq!(split_user("alice@corp.example", ""), ("alice", "corp.example"));
        assert_eq!(split_user("CORP\\alice", "OTHER"), ("CORP\\alice", "OTHER"));
        assert_eq!(split_user("a\\b@c", ""), ("a\\b@c", ""));
        assert_eq!(split_user("alice", ""), ("alice", ""));
        assert_eq!(target_name("https://h.example/x"), "HTTP/h.example");
        assert_eq!(target_name("https://h.example:8443/x"), "HTTP/h.example:8443");
    }

    #[test]
    fn bare_ntlm_challenge_starts_handshake() {
        let mut req = PreparedRequest {
            url: "http://intranet/".into(),
            auth: Auth::new(AuthType::Ntlm).with("username", "u"),
            ..Default::default()
        };
        let headers = vec![("www-authenticate".to_string(), "NTLM".to_string())];
        assert!(handle_challenge(&mut req, &headers, None).unwrap());
        assert_eq!(sent_message_type(&req), Some(1));
    }

    #[test]
    fn malformed_challenges_are_errors() {
        assert!(parse_challenge(b"NTLMSSP\0\x02\0\0\0").is_err());
        let mut raw = challenge_bytes(NEGOTIATE_UNICODE, &[]);
        raw[40..42].copy_from_slice(&500u16.to_le_bytes());
        assert!(parse_challenge(&raw).is_err());
        // Truncated AV pairs are ignored rather than read out of bounds.
        assert_eq!(av_pairs(&[2, 0, 9, 0, 1]), Vec::<(u16, &[u8])>::new());
    }
}
