//! Web Proxy Auto-Discovery (WPAD) over DNS, for the system proxy when the
//! OS settings ask for automatic proxy discovery (macOS "Auto Proxy
//! Discovery", Windows "Automatically detect settings") and set no PAC URL.
//!
//! The candidates are `http://wpad.<domain>/wpad.dat` for each DNS search
//! domain of the machine (and its own domain), in order, each used as it
//! is: there is no devolution to parent domains. That is what Chrome does
//! (it asks the resolver for the unqualified `wpad`, which only appends the
//! search domains) and Firefox no longer devolves either, because
//! `wpad.<tld>` and `wpad.co.uk`-style names can be registered by anyone.
//! For the same reason a search domain that is itself a public suffix
//! (`com`, `co.uk`, `github.io`) is skipped, and so is `local`, whose names
//! are answered by any machine on the link (mDNS). DHCP option 252 is not
//! used. The first candidate that serves a script wins; each is given
//! [`WPAD_TIMEOUT`], and results (failures too) are cached like other PAC
//! downloads.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use super::pac::Pac;
use super::{PacSource, load_pac_via};

/// How long one candidate may take to answer (DNS lookup included).
pub(crate) const WPAD_TIMEOUT: Duration = Duration::from_secs(2);

/// The candidate script URLs for the search domains `domains`.
pub(crate) fn candidates(domains: &[String]) -> Vec<String> {
    use publicsuffix::Psl as _;
    let suffixes = crate::cookies::public_suffixes();
    let mut out: Vec<String> = Vec::new();
    for domain in domains {
        let d = domain.trim().trim_end_matches('.').to_ascii_lowercase();
        let valid = !d.is_empty()
            && d.split('.')
                .all(|l| !l.is_empty() && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
            && d.parse::<std::net::IpAddr>().is_err();
        if !valid || d == "local" {
            continue;
        }
        if suffixes.suffix(d.as_bytes()).is_some_and(|s| s.is_known() && s.as_bytes() == d.as_bytes()) {
            continue;
        }
        let url = format!("http://wpad.{d}/wpad.dat");
        if !out.contains(&url) {
            out.push(url);
        }
    }
    out
}

/// The first candidate for `domains` that serves a PAC script: its URL and
/// the script. `resolve_to` replaces the DNS lookups (tests).
pub(crate) fn discover(domains: &[String], resolve_to: Option<SocketAddr>) -> Option<(String, Arc<Pac>)> {
    for url in candidates(domains) {
        match load_pac_via(&PacSource::Wpad(url.clone()), resolve_to) {
            Ok(pac) => {
                tracing::debug!("proxy auto-discovery: using {url}");
                return Some((url, pac));
            }
            Err(e) => tracing::debug!("proxy auto-discovery: {url}: {e}"),
        }
    }
    None
}

/// The `search` (or `domain`) list of a resolv.conf; the last such line
/// wins, as for the system resolver.
#[cfg_attr(any(target_os = "macos", windows), allow(dead_code))]
pub(crate) fn parse_resolv_conf(text: &str) -> Vec<String> {
    let mut domains = Vec::new();
    for line in text.lines() {
        let line = line.split(['#', ';']).next().unwrap_or_default();
        let mut words = line.split_whitespace();
        if let Some("search" | "domain") = words.next() {
            domains = words.map(str::to_string).collect();
        }
    }
    domains
}

/// Whether the "Automatically detect settings" flag is set in a Windows
/// `Internet Settings\Connections\DefaultConnectionSettings` value. Its
/// layout (undocumented, stable since Windows XP): a DWORD version (0x3C,
/// 0x46 since Windows 7), a DWORD change counter, then a DWORD of
/// `PROXY_TYPE_*` flags (wininet.h): 0x01 direct, 0x02 manual proxy, 0x04
/// setup script (`AutoConfigURL`), 0x08 auto-detect; then the length-prefixed
/// proxy server, bypass list and script URL strings.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn auto_detect_flag(settings: &[u8]) -> bool {
    const PROXY_TYPE_AUTO_DETECT: u32 = 0x08;
    match settings.get(8..12) {
        Some(flags) => u32::from_le_bytes([flags[0], flags[1], flags[2], flags[3]]) & PROXY_TYPE_AUTO_DETECT != 0,
        None => false,
    }
}

/// The machine's DNS search domains (macOS: the global DNS configuration of
/// the dynamic store, `SearchDomains` then `DomainName`).
#[cfg(target_os = "macos")]
pub(crate) fn search_domains() -> Vec<String> {
    use system_configuration::core_foundation::array::CFArray;
    use system_configuration::core_foundation::base::{CFType, TCFType};
    use system_configuration::core_foundation::dictionary::CFDictionary;
    use system_configuration::core_foundation::string::CFString;
    use system_configuration::dynamic_store::SCDynamicStoreBuilder;
    use system_configuration::sys::schema_definitions::{kSCPropNetDNSDomainName, kSCPropNetDNSSearchDomains};
    let Some(store) = SCDynamicStoreBuilder::new("merpati").build() else { return Vec::new() };
    let Some(dict) = store.get("State:/Network/Global/DNS").and_then(|p| p.downcast_into::<CFDictionary>()) else {
        return Vec::new();
    };
    // SAFETY: the dictionary returned by the store, retained by `wrap_under_get_rule`.
    let dict: CFDictionary<CFString, CFType> = unsafe { CFDictionary::wrap_under_get_rule(dict.as_concrete_TypeRef()) };
    let mut domains = Vec::new();
    // SAFETY: framework constants, valid for the life of the process.
    let (search, own) = unsafe { (kSCPropNetDNSSearchDomains, kSCPropNetDNSDomainName) };
    if let Some(list) = dict.find(search).and_then(|v| v.downcast::<CFArray>()) {
        for item in list.get_all_values() {
            // SAFETY: the items of a CFArray owned by `list`, retained by `wrap_under_get_rule`.
            if let Some(s) = unsafe { CFType::wrap_under_get_rule(item) }.downcast::<CFString>() {
                domains.push(s.to_string());
            }
        }
    }
    if let Some(s) = dict.find(own).and_then(|v| v.downcast::<CFString>()) {
        domains.push(s.to_string());
    }
    domains
}

/// The machine's DNS search domains (Windows: the suffix search list, the
/// primary DNS suffix and the domains set by DHCP or per adapter).
#[cfg(windows)]
pub(crate) fn search_domains() -> Vec<String> {
    let mut domains = Vec::new();
    let mut add = |value: String| domains.extend(value.split([',', ' ']).filter(|d| !d.is_empty()).map(String::from));
    let policy = "SOFTWARE\\Policies\\Microsoft\\Windows NT\\DNSClient";
    if let Ok(v) = windows_registry::LOCAL_MACHINE.open(policy).and_then(|k| k.get_string("SearchList")) {
        add(v);
    }
    let tcpip = "SYSTEM\\CurrentControlSet\\Services\\Tcpip\\Parameters";
    if let Ok(key) = windows_registry::LOCAL_MACHINE.open(tcpip) {
        for name in ["SearchList", "Domain", "DhcpDomain"] {
            if let Ok(v) = key.get_string(name) {
                add(v);
            }
        }
        if let Ok(adapters) = key.open("Interfaces") {
            for id in adapters.keys().into_iter().flatten() {
                if let Ok(adapter) = adapters.open(&id) {
                    for name in ["Domain", "DhcpDomain"] {
                        if let Ok(v) = adapter.get_string(name) {
                            add(v);
                        }
                    }
                }
            }
        }
    }
    domains
}

/// The machine's DNS search domains (`/etc/resolv.conf`).
#[cfg(not(any(target_os = "macos", windows)))]
pub(crate) fn search_domains() -> Vec<String> {
    std::fs::read_to_string("/etc/resolv.conf").map(|t| parse_resolv_conf(&t)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn domains(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn candidates_use_each_domain_as_is() {
        assert_eq!(
            candidates(&domains(&["Corp.Example.com.", "eu.corp.example.com", "corp.example.com", "lan"])),
            [
                "http://wpad.corp.example.com/wpad.dat",
                "http://wpad.eu.corp.example.com/wpad.dat",
                "http://wpad.lan/wpad.dat",
            ]
        );
        // Never a parent domain: `example.com` is not tried for `corp.example.com`.
        assert_eq!(candidates(&domains(&["a.b.example.org"])), ["http://wpad.a.b.example.org/wpad.dat"]);
    }

    #[test]
    fn unsafe_domains_are_skipped() {
        let skipped =
            ["com", "co.uk", "github.io", "local", "", " ", "10.0.0.1", "::1", "a..b", "bad/domain", "x y", "évil.fr"];
        assert_eq!(candidates(&domains(&skipped)), Vec::<String>::new());
        // Registrable domains under a public suffix are fine.
        assert_eq!(candidates(&domains(&["example.co.uk"])), ["http://wpad.example.co.uk/wpad.dat"]);
    }

    #[test]
    fn resolv_conf_search_lists() {
        let conf = "# generated\nnameserver 10.0.0.1\nsearch corp.example.com example.com ; comment\noptions ndots:2\n";
        assert_eq!(parse_resolv_conf(conf), ["corp.example.com", "example.com"]);
        // The last search/domain line wins.
        assert_eq!(parse_resolv_conf("search a.test b.test\ndomain c.test\n"), ["c.test"]);
        assert_eq!(parse_resolv_conf("domain c.test\nsearch a.test\n"), ["a.test"]);
        assert_eq!(parse_resolv_conf("nameserver 1.1.1.1\n"), Vec::<String>::new());
    }

    #[test]
    fn windows_connection_settings() {
        // Version 0x46, counter 5, flags direct | auto-detect, then empty strings.
        let mut blob = vec![0x46, 0, 0, 0, 5, 0, 0, 0, 0x09, 0, 0, 0];
        blob.extend_from_slice(&[0; 12]);
        assert!(auto_detect_flag(&blob));
        blob[8] = 0x01 | 0x02 | 0x04;
        assert!(!auto_detect_flag(&blob));
        assert!(!auto_detect_flag(&blob[..10]));
        assert!(!auto_detect_flag(&[]));
    }

    #[test]
    fn search_domains_can_be_read() {
        // Whatever the machine's configuration, reading it never fails.
        for d in search_domains() {
            assert!(!d.is_empty());
        }
    }
}
