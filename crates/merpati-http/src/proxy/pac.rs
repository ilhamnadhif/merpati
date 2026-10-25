//! Proxy auto-config (PAC): a `FindProxyForURL(url, host)` script, from the
//! system settings or the custom proxy settings, chooses the proxy per
//! request.
//!
//! The script runs in its own JavaScript context on a dedicated thread with
//! the standard helper functions (`shExpMatch`, `isInNet`, `dnsResolve`,
//! ...) and nothing else: no network, files or timers. Results are cached
//! per scheme, host and port. Every entry of the result is kept
//! (`PROXY a:8080; PROXY b:8080; DIRECT`): like browsers, the next one is
//! used when a proxy cannot be reached (see `ProxyResolver`).

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::mpsc;
use std::time::Duration;

use boa_engine::{Context, JsResult, JsString, JsValue, NativeFunction, Source};
use parking_lot::Mutex;

use super::{ProxyError, ProxyErrorKind};

/// The largest PAC script that is loaded.
const MAX_SCRIPT_BYTES: usize = 1024 * 1024;
/// How long loading the script may take.
pub(crate) const LOAD_TIMEOUT: Duration = Duration::from_secs(10);
/// How long one `FindProxyForURL` call may take (DNS lookups included).
const CALL_TIMEOUT: Duration = Duration::from_secs(5);
/// Cached decisions per scheme, host and port.
const CACHE_ENTRIES: usize = 512;

/// What the script chose for a URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PacChoice {
    Direct,
    /// `http://`, `https://`, `socks4://` or `socks5h://` proxy URL.
    Proxy(url::Url),
}

/// A loaded PAC script.
pub(crate) struct Pac {
    jobs: Mutex<mpsc::Sender<Job>>,
    cache: Mutex<HashMap<String, Vec<PacChoice>>>,
}

struct Job {
    url: String,
    host: String,
    reply: mpsc::Sender<Result<String, String>>,
}

impl std::fmt::Debug for Pac {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Pac")
    }
}

impl Pac {
    /// Loads the script at `location` (`http://`, `https://` or `file://`
    /// URL, or a file path) and compiles it. Downloads give up after
    /// `timeout`; `resolve_to` replaces the DNS lookup of the URL's host
    /// (tests).
    pub(crate) fn load(location: &str, timeout: Duration, resolve_to: Option<SocketAddr>) -> Result<Pac, ProxyError> {
        let source = fetch(location.trim(), timeout, resolve_to)?;
        Pac::from_source(source)
    }

    /// Compiles `source` on the script thread; fails when it does not
    /// define `FindProxyForURL`.
    pub(crate) fn from_source(source: String) -> Result<Pac, ProxyError> {
        let (tx, rx) = mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        std::thread::Builder::new()
            .name("proxy-pac".into())
            .spawn(move || {
                let mut ctx = match compile(&source) {
                    Ok(ctx) => {
                        let _ = ready_tx.send(Ok(()));
                        ctx
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                while let Ok(job) = rx.recv() {
                    let _ = job.reply.send(call(&mut ctx, &job.url, &job.host));
                }
            })
            .map_err(|e| config_error(format!("cannot start the script thread: {e}")))?;
        match ready_rx.recv_timeout(LOAD_TIMEOUT) {
            Ok(Ok(())) => Ok(Pac { jobs: Mutex::new(tx), cache: Mutex::new(HashMap::new()) }),
            Ok(Err(e)) => Err(config_error(e)),
            Err(_) => Err(config_error("the script took too long to load".into())),
        }
    }

    /// The choices for a connection to `host:port`, in the script's order
    /// (`url` is the full URL passed to the script); never empty. Errors
    /// and timeouts connect directly, as browsers do.
    pub(crate) fn choose(&self, url: &str, scheme: &str, host: &str, port: u16) -> Vec<PacChoice> {
        let key = format!("{scheme}://{host}:{port}");
        if let Some(c) = self.cache.lock().get(&key) {
            return c.clone();
        }
        let (reply, answer) = mpsc::channel();
        let job = Job { url: url.to_string(), host: host.to_string(), reply };
        if self.jobs.lock().send(job).is_err() {
            return vec![PacChoice::Direct];
        }
        let choice = match answer.recv_timeout(CALL_TIMEOUT) {
            Ok(Ok(result)) => Some(parse_result(&result)).filter(|c| !c.is_empty()).unwrap_or(vec![PacChoice::Direct]),
            Ok(Err(e)) => {
                tracing::warn!("FindProxyForURL failed: {e}");
                vec![PacChoice::Direct]
            }
            Err(_) => {
                tracing::warn!("FindProxyForURL took too long");
                vec![PacChoice::Direct]
            }
        };
        let mut cache = self.cache.lock();
        if cache.len() >= CACHE_ENTRIES {
            cache.clear();
        }
        cache.insert(key, choice.clone());
        choice
    }
}

fn config_error(message: String) -> ProxyError {
    ProxyError::new(ProxyErrorKind::Config, format!("Proxy auto-config script: {message}"))
}

/// The script text. HTTP(S) scripts are fetched directly (never through a
/// proxy) on a separate thread, so this can be called from async code.
fn fetch(location: &str, timeout: Duration, resolve_to: Option<SocketAddr>) -> Result<String, ProxyError> {
    let path = match url::Url::parse(location) {
        Ok(u) if matches!(u.scheme(), "http" | "https") => return fetch_http(u, timeout, resolve_to),
        Ok(u) if u.scheme() == "file" => {
            u.to_file_path().map_err(|_| config_error(format!("invalid file URL {location}")))?
        }
        _ => std::path::PathBuf::from(location),
    };
    let bytes = std::fs::read(&path).map_err(|e| config_error(format!("cannot read {}: {e}", path.display())))?;
    if bytes.len() > MAX_SCRIPT_BYTES {
        return Err(config_error("the script is larger than 1 MB".into()));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn fetch_http(url: url::Url, timeout: Duration, resolve_to: Option<SocketAddr>) -> Result<String, ProxyError> {
    // The URL may carry credentials; errors name only the host.
    let host = url.host_str().unwrap_or_default().to_string();
    let resolve_host = host.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("proxy-pac-fetch".into())
        .spawn(move || {
            let result = (|| -> Result<String, String> {
                let rt =
                    tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
                rt.block_on(async {
                    let mut builder = reqwest::Client::builder().no_proxy().timeout(timeout);
                    if let Some(addr) = resolve_to {
                        builder = builder.resolve(&resolve_host, addr);
                    }
                    let client = builder.build().map_err(|e| e.without_url().to_string())?;
                    let mut resp = client.get(url).send().await.map_err(|e| e.without_url().to_string())?;
                    if !resp.status().is_success() {
                        return Err(format!("HTTP {}", resp.status()));
                    }
                    let mut body = Vec::new();
                    while let Some(chunk) = resp.chunk().await.map_err(|e| e.without_url().to_string())? {
                        body.extend_from_slice(&chunk);
                        if body.len() > MAX_SCRIPT_BYTES {
                            return Err("the script is larger than 1 MB".into());
                        }
                    }
                    Ok(String::from_utf8_lossy(&body).into_owned())
                })
            })();
            let _ = tx.send(result);
        })
        .map_err(|e| config_error(format!("cannot start the download: {e}")))?;
    match rx.recv_timeout(timeout + Duration::from_secs(1)) {
        Ok(Ok(text)) => Ok(text),
        Ok(Err(e)) => Err(config_error(format!("cannot download it from {host}: {e}"))),
        Err(_) => Err(config_error(format!("downloading it from {host} took too long"))),
    }
}

/// A context with the helper functions and the script evaluated.
fn compile(source: &str) -> Result<Context, String> {
    let mut ctx = Context::default();
    {
        let limits = ctx.runtime_limits_mut();
        limits.set_loop_iteration_limit(10_000_000);
        limits.set_recursion_limit(400);
    }
    type HostFn = fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>;
    let fns: [(&str, usize, HostFn); 3] =
        [("__pac_resolve", 2, host_resolve), ("__pac_my_ip", 1, host_my_ip), ("__pac_in_net", 2, host_in_net)];
    for (name, len, f) in fns {
        ctx.register_global_builtin_callable(JsString::from(name), len, NativeFunction::from_fn_ptr(f))
            .map_err(|e| e.to_string())?;
    }
    ctx.eval(Source::from_bytes(HELPERS.as_bytes())).map_err(|e| e.to_string())?;
    ctx.eval(Source::from_bytes(source.as_bytes())).map_err(|e| e.to_string())?;
    let defined =
        ctx.global_object().get(JsString::from("FindProxyForURL"), &mut ctx).map(|f| f.is_callable()).unwrap_or(false);
    if !defined {
        return Err("it does not define FindProxyForURL(url, host)".into());
    }
    Ok(ctx)
}

fn call(ctx: &mut Context, url: &str, host: &str) -> Result<String, String> {
    let f = ctx.global_object().get(JsString::from("FindProxyForURL"), ctx).map_err(|e| e.to_string())?;
    let f = f.as_callable().ok_or("FindProxyForURL is not a function")?;
    let result = f
        .call(&JsValue::undefined(), &[JsString::from(url).into(), JsString::from(host).into()], ctx)
        .map_err(|e| e.to_string())?;
    if result.is_null_or_undefined() {
        return Ok("DIRECT".into());
    }
    result.to_string(ctx).map(|s| s.to_std_string_escaped()).map_err(|e| e.to_string())
}

fn arg_string(args: &[JsValue], i: usize, ctx: &mut Context) -> JsResult<String> {
    match args.get(i) {
        Some(v) if !v.is_null_or_undefined() => Ok(v.to_string(ctx)?.to_std_string_escaped()),
        _ => Ok(String::new()),
    }
}

/// `__pac_resolve(host, all)`: the first IPv4 address of `host` (any
/// address when there is none), or with `all` every address separated by
/// `;`; "" when it does not resolve.
fn host_resolve(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let host = arg_string(args, 0, ctx)?;
    let all = args.get(1).is_some_and(JsValue::to_boolean);
    let addrs = resolve(&host);
    let text = if all {
        addrs.iter().map(IpAddr::to_string).collect::<Vec<_>>().join(";")
    } else {
        addrs.iter().find(|a| a.is_ipv4()).or(addrs.first()).map(IpAddr::to_string).unwrap_or_default()
    };
    Ok(JsString::from(text).into())
}

fn resolve(host: &str) -> Vec<IpAddr> {
    let host = host.trim().trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() {
        return Vec::new();
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return vec![ip];
    }
    (host, 0).to_socket_addrs().map(|a| a.map(|s| s.ip()).collect()).unwrap_or_default()
}

/// `__pac_my_ip(all)`: the address of the interface with the default
/// route (IPv4, or with `all` IPv4 and IPv6 separated by `;`), "" when
/// there is none. Connecting a UDP socket sends nothing; it only selects
/// the route.
fn host_my_ip(_: &JsValue, args: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    let all = args.first().is_some_and(JsValue::to_boolean);
    let route = |bind: &str, target: &str| -> Option<IpAddr> {
        let socket = UdpSocket::bind(bind).ok()?;
        socket.connect(target).ok()?;
        Some(socket.local_addr().ok()?.ip()).filter(|ip| !ip.is_unspecified())
    };
    let v4 = route("0.0.0.0:0", "192.0.2.1:9");
    let text = if all {
        [v4, route("[::]:0", "[2001:db8::1]:9")]
            .into_iter()
            .flatten()
            .map(|ip| ip.to_string())
            .collect::<Vec<_>>()
            .join(";")
    } else {
        v4.map(|ip| ip.to_string()).unwrap_or_default()
    };
    Ok(JsString::from(text).into())
}

/// `__pac_in_net(ip, "prefix/bits")` for `isInNetEx`, IPv4 and IPv6.
fn host_in_net(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let ip = arg_string(args, 0, ctx)?;
    let prefix = arg_string(args, 1, ctx)?;
    Ok(in_net(&ip, &prefix).into())
}

fn in_net(ip: &str, prefix: &str) -> bool {
    let Some((net, bits)) = prefix.trim().split_once('/') else { return false };
    let (Ok(ip), Ok(net), Ok(bits)) = (ip.trim().parse::<IpAddr>(), net.parse::<IpAddr>(), bits.parse::<u32>()) else {
        return false;
    };
    match (ip, net) {
        (IpAddr::V4(a), IpAddr::V4(b)) if bits <= 32 => {
            let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
            u32::from(a) & mask == u32::from(b) & mask
        }
        (IpAddr::V6(a), IpAddr::V6(b)) if bits <= 128 => {
            let mask = if bits == 0 { 0 } else { u128::MAX << (128 - bits) };
            u128::from(a) & mask == u128::from(b) & mask
        }
        _ => false,
    }
}

/// The entries of a `FindProxyForURL` result, unusable ones skipped.
pub(crate) fn parse_result(result: &str) -> Vec<PacChoice> {
    result
        .split(';')
        .filter_map(|entry| {
            let mut parts = entry.split_whitespace();
            let kind = parts.next()?.to_ascii_uppercase();
            if kind == "DIRECT" {
                return Some(PacChoice::Direct);
            }
            let scheme = match kind.as_str() {
                "PROXY" | "HTTP" => "http",
                "HTTPS" => "https",
                // As in Chrome: SOCKS is SOCKS4; SOCKS5 resolves names on the proxy.
                "SOCKS" | "SOCKS4" => "socks4",
                "SOCKS5" => "socks5h",
                _ => return None,
            };
            let authority = parts.next()?;
            url::Url::parse(&format!("{scheme}://{authority}")).ok().filter(|u| u.has_host()).map(PacChoice::Proxy)
        })
        .collect()
}

/// The standard PAC helper functions (Netscape's original set and the
/// Microsoft `Ex` extensions), on top of the native `__pac_*` functions.
const HELPERS: &str = r#"
function isPlainHostName(host) {
  host = String(host);
  return host.indexOf('.') < 0 && host.indexOf(':') < 0;
}
function dnsDomainIs(host, domain) {
  host = String(host).toLowerCase();
  domain = String(domain).toLowerCase();
  return host.length >= domain.length && host.substring(host.length - domain.length) === domain;
}
function localHostOrDomainIs(host, hostdom) {
  host = String(host).toLowerCase();
  hostdom = String(hostdom).toLowerCase();
  return host === hostdom || hostdom.indexOf(host + '.') === 0;
}
function dnsDomainLevels(host) {
  return String(host).split('.').length - 1;
}
function isResolvable(host) {
  return __pac_resolve(String(host), false) !== '';
}
function isResolvableEx(host) {
  return __pac_resolve(String(host), true) !== '';
}
function dnsResolve(host) {
  var ip = __pac_resolve(String(host), false);
  return ip === '' ? null : ip;
}
function dnsResolveEx(host) {
  return __pac_resolve(String(host), true);
}
function myIpAddress() {
  return __pac_my_ip(false) || '127.0.0.1';
}
function myIpAddressEx() {
  return __pac_my_ip(true);
}
function convert_addr(ipchars) {
  var p = String(ipchars).split('.');
  return (((p[0] & 255) << 24) | ((p[1] & 255) << 16) | ((p[2] & 255) << 8) | (p[3] & 255)) >>> 0;
}
function __pac_is_ipv4(s) {
  return /^\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}$/.test(s);
}
function isInNet(host, pattern, mask) {
  host = String(host);
  var ip = __pac_is_ipv4(host) ? host : dnsResolve(host);
  if (ip === null || !__pac_is_ipv4(ip)) return false;
  var m = convert_addr(mask);
  return ((convert_addr(ip) & m) >>> 0) === ((convert_addr(pattern) & m) >>> 0);
}
function isInNetEx(host, prefix) {
  host = String(host);
  var ips = /^[0-9a-fA-F:.]+$/.test(host) && (host.indexOf(':') >= 0 || __pac_is_ipv4(host))
    ? [host] : dnsResolveEx(host).split(';');
  for (var i = 0; i < ips.length; i++) {
    if (ips[i] !== '' && __pac_in_net(ips[i], String(prefix))) return true;
  }
  return false;
}
function sortIpAddressList(list) {
  var ips = String(list).split(';').filter(function (s) { return s !== ''; });
  return ips.sort(function (a, b) {
    var a6 = a.indexOf(':') >= 0, b6 = b.indexOf(':') >= 0;
    if (a6 !== b6) return a6 ? -1 : 1;
    return a6 ? (a < b ? -1 : a > b ? 1 : 0) : convert_addr(a) - convert_addr(b);
  }).join(';');
}
function getClientVersion() {
  return '1.0';
}
function shExpMatch(str, shexp) {
  var re = String(shexp).replace(/[.+^${}()|[\]\\\/]/g, '\\$&').replace(/\*/g, '.*').replace(/\?/g, '.');
  return new RegExp('^' + re + '$').test(String(str));
}
function alert() {}

var __pac_days = ['SUN', 'MON', 'TUE', 'WED', 'THU', 'FRI', 'SAT'];
var __pac_months = ['JAN', 'FEB', 'MAR', 'APR', 'MAY', 'JUN', 'JUL', 'AUG', 'SEP', 'OCT', 'NOV', 'DEC'];

// Arguments without a trailing "GMT", and whether it was there.
function __pac_args(args) {
  var list = Array.prototype.slice.call(args);
  var gmt = list.length > 0 && String(list[list.length - 1]).toUpperCase() === 'GMT';
  if (gmt) list.pop();
  return { list: list, gmt: gmt };
}
// Inclusive range test that wraps around (FRI..MON, NOV..FEB, 22..6).
function __pac_within(v, lo, hi) {
  return lo <= hi ? (v >= lo && v <= hi) : (v >= lo || v <= hi);
}
function weekdayRange() {
  var a = __pac_args(arguments);
  var now = new Date();
  var day = a.gmt ? now.getUTCDay() : now.getDay();
  var lo = __pac_days.indexOf(String(a.list[0]).toUpperCase());
  if (lo < 0) return false;
  if (a.list.length < 2) return day === lo;
  var hi = __pac_days.indexOf(String(a.list[1]).toUpperCase());
  return hi >= 0 && __pac_within(day, lo, hi);
}
function dateRange() {
  var a = __pac_args(arguments);
  var now = new Date();
  var cur = {
    d: a.gmt ? now.getUTCDate() : now.getDate(),
    m: a.gmt ? now.getUTCMonth() : now.getMonth(),
    y: a.gmt ? now.getUTCFullYear() : now.getFullYear()
  };
  // Each argument is a day (1-31), a month name or a year.
  var parts = a.list.map(function (v) {
    var m = __pac_months.indexOf(String(v).toUpperCase());
    if (m >= 0) return { k: 'm', v: m };
    var n = Number(v);
    if (isNaN(n)) return { k: '?', v: 0 };
    return n > 31 ? { k: 'y', v: n } : { k: 'd', v: n };
  });
  if (parts.length === 0 || parts.length > 6 || parts.some(function (p) { return p.k === '?'; })) return false;
  if (parts.length === 1) return cur[parts[0].k] === parts[0].v;
  if (parts.length % 2 !== 0) return false;
  var half = parts.length / 2;
  var lo = parts.slice(0, half), hi = parts.slice(half);
  var kinds = lo.map(function (p) { return p.k; }).join('');
  if (kinds !== hi.map(function (p) { return p.k; }).join('')) return false;
  // Compare as one number, largest unit first: year, month, day.
  function key(ps) {
    var o = { y: 0, m: 0, d: 0 };
    ps.forEach(function (p) { o[p.k] = p.v; });
    return o.y * 416 + o.m * 32 + o.d;
  }
  var now_ = key(kinds.split('').map(function (k) { return { k: k, v: cur[k] }; }));
  var from = key(lo), to = key(hi);
  return kinds.indexOf('y') >= 0 ? (now_ >= from && now_ <= to) : __pac_within(now_, from, to);
}
function timeRange() {
  var a = __pac_args(arguments);
  var now = new Date();
  var h = a.gmt ? now.getUTCHours() : now.getHours();
  var m = a.gmt ? now.getUTCMinutes() : now.getMinutes();
  var s = a.gmt ? now.getUTCSeconds() : now.getSeconds();
  var n = a.list.map(Number);
  if (n.some(isNaN)) return false;
  var t = h * 3600 + m * 60 + s;
  switch (n.length) {
    case 1: return h === n[0];
    case 2: return __pac_within(h, n[0], n[1]);
    case 4: return __pac_within(t, n[0] * 3600 + n[1] * 60, n[2] * 3600 + n[3] * 60 + 59);
    case 6: return __pac_within(t, n[0] * 3600 + n[1] * 60 + n[2], n[3] * 3600 + n[4] * 60 + n[5]);
    default: return false;
  }
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(source: &str, url: &str, host: &str) -> String {
        let mut ctx = compile(source).unwrap();
        call(&mut ctx, url, host).unwrap()
    }

    #[test]
    fn results() {
        let proxy = |u: &str| PacChoice::Proxy(url::Url::parse(u).unwrap());
        assert_eq!(parse_result("DIRECT"), [PacChoice::Direct]);
        assert_eq!(
            parse_result("PROXY a.example:8080; SOCKS5 b:1080;DIRECT"),
            [proxy("http://a.example:8080"), proxy("socks5h://b:1080"), PacChoice::Direct]
        );
        assert_eq!(parse_result("  https  p:443 "), [proxy("https://p:443")]);
        assert_eq!(parse_result("SOCKS s:1080"), [proxy("socks4://s:1080")]);
        assert_eq!(parse_result("QUIC q:443; PROXY"), []);
        assert_eq!(parse_result(""), []);
    }

    #[test]
    fn helpers() {
        let script = r#"
            function FindProxyForURL(url, host) {
              var out = [
                isPlainHostName(host), dnsDomainIs(host, '.example.com'), localHostOrDomainIs('www', host),
                dnsDomainLevels(host), shExpMatch(url, 'http://*.example.com/a?c*'), shExpMatch(host, '*.com'),
                isInNet('10.1.2.3', '10.0.0.0', '255.0.0.0'), isInNet('11.1.2.3', '10.0.0.0', '255.0.0.0'),
                isInNetEx('10.1.2.3', '10.0.0.0/8'), isInNetEx('2001:db8::5', '2001:db8::/32'),
                dnsResolve('127.0.0.1'), isResolvable('nonexistent.invalid'), sortIpAddressList('10.0.0.2;10.0.0.1;::1'),
                weekdayRange('SUN', 'SAT'), dateRange(1, 31), timeRange(0, 23), typeof myIpAddress()
              ];
              return out.join(',');
            }
        "#;
        assert_eq!(
            eval(script, "http://www.example.com/abc", "www.example.com"),
            "false,true,true,2,true,true,true,false,true,true,127.0.0.1,false,::1;10.0.0.1;10.0.0.2,true,true,true,string"
        );
    }

    #[test]
    fn date_and_time_ranges() {
        let script = r#"
            function FindProxyForURL() {
              return [
                weekdayRange('XYZ'), dateRange('JAN', 'DEC'), dateRange(1990, 2999), dateRange(1, 'JAN', 31, 'DEC'),
                dateRange('JAN', 1990, 'DEC', 2999), dateRange(1, 'JAN', 1990, 31, 'DEC', 2999),
                dateRange(1, 'JAN', 1990, 31, 'DEC', 1991), timeRange(0, 0, 23, 59), timeRange(0, 0, 0, 23, 59, 59, 'GMT'),
                dateRange(1, 2, 3)
              ].join(',');
            }
        "#;
        assert_eq!(eval(script, "http://a/", "a"), "false,true,true,true,true,true,false,true,true,false");
    }

    #[test]
    fn scripts_must_define_the_function_and_cannot_hang() {
        assert!(compile("var x = 1;").unwrap_err().contains("FindProxyForURL"));
        assert!(compile("function FindProxyForURL( {").is_err());
        let mut ctx = compile("function FindProxyForURL() { while (true) {} }").unwrap();
        assert!(call(&mut ctx, "http://a/", "a").is_err());
        // No network, files or timers in the script's context.
        let probe = "function FindProxyForURL() { return [typeof fetch, typeof require, typeof setTimeout, typeof pm].join(); }";
        assert_eq!(eval(probe, "http://a/", "a"), "undefined,undefined,undefined,undefined");
    }

    #[test]
    fn choices_are_cached_and_errors_go_direct() {
        let pac = Pac::from_source(
            "var calls = 0; function FindProxyForURL(url, host) { calls++; \
             if (host === 'boom') throw new Error('x'); return 'PROXY p' + calls + ':3128; DIRECT'; }"
                .into(),
        )
        .unwrap();
        let proxy = |u: &str| PacChoice::Proxy(url::Url::parse(u).unwrap());
        let first = pac.choose("http://a.test/x", "http", "a.test", 80);
        assert_eq!(first, [proxy("http://p1:3128"), PacChoice::Direct]);
        assert_eq!(pac.choose("http://a.test/y", "http", "a.test", 80), first);
        assert_eq!(pac.choose("https://a.test/", "https", "a.test", 443), [proxy("http://p2:3128"), PacChoice::Direct]);
        assert_eq!(pac.choose("http://boom/", "http", "boom", 80), [PacChoice::Direct]);
        // An answer without a usable entry connects directly too.
        let odd = Pac::from_source("function FindProxyForURL() { return 'QUIC q:443; PROXY'; }".into()).unwrap();
        assert_eq!(odd.choose("http://a.test/", "http", "a.test", 80), [PacChoice::Direct]);
        assert!(Pac::from_source("nope(".into()).is_err());
    }

    #[test]
    fn in_net_prefixes() {
        assert!(in_net("192.168.1.7", "192.168.0.0/16"));
        assert!(!in_net("192.169.1.7", "192.168.0.0/16"));
        assert!(in_net("1.2.3.4", "0.0.0.0/0"));
        assert!(in_net("fe80::1", "fe80::/10"));
        assert!(!in_net("fe80::1", "10.0.0.0/8"));
        assert!(!in_net("1.2.3.4", "1.2.3.4/33"));
        assert!(!in_net("x", "1.2.3.4/8"));
    }
}
