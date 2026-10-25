//! Parsing of `WWW-Authenticate` and `Proxy-Authenticate` headers (RFC
//! 7235), including several challenges in one header and token68 values
//! such as NTLM messages. Shared by the auth schemes (`merpati-auth`) and
//! the proxy tunnels.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Challenge {
    /// Lowercased scheme name (`digest`, `ntlm`, `basic`).
    pub scheme: String,
    pub token68: Option<String>,
    pub params: Vec<(String, String)>,
}

impl Challenge {
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

fn is_tchar(c: char) -> bool {
    c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c)
}

struct Lexer<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> Lexer<'a> {
    fn skip_ws(&mut self) {
        while self.i < self.s.len() && (self.s[self.i] == b' ' || self.s[self.i] == b'\t') {
            self.i += 1;
        }
    }

    fn skip_ws_commas(&mut self) {
        while self.i < self.s.len() && matches!(self.s[self.i], b' ' | b'\t' | b',') {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn token(&mut self) -> String {
        let start = self.i;
        while self.i < self.s.len() && is_tchar(self.s[self.i] as char) {
            self.i += 1;
        }
        String::from_utf8_lossy(&self.s[start..self.i]).into_owned()
    }

    fn quoted(&mut self) -> String {
        // Assumes current char is '"'.
        self.i += 1;
        let mut out = Vec::new();
        while self.i < self.s.len() {
            match self.s[self.i] {
                b'\\' if self.i + 1 < self.s.len() => {
                    out.push(self.s[self.i + 1]);
                    self.i += 2;
                }
                b'"' => {
                    self.i += 1;
                    break;
                }
                c => {
                    out.push(c);
                    self.i += 1;
                }
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// True if, from the current position, a `name=` auth-param follows.
    fn at_param(&self) -> bool {
        let mut j = self.i;
        while j < self.s.len() && is_tchar(self.s[j] as char) {
            j += 1;
        }
        if j == self.i {
            return false;
        }
        while j < self.s.len() && (self.s[j] == b' ' || self.s[j] == b'\t') {
            j += 1;
        }
        // `name=` but not `token68==` padding (`=` followed by `=`, `,` or end).
        j < self.s.len() && self.s[j] == b'=' && !matches!(self.s.get(j + 1), Some(b'=') | Some(b',') | None)
    }
}

/// Parses every challenge from every `WWW-Authenticate` (or
/// `Proxy-Authenticate`) value.
pub fn parse_challenges<'a>(values: impl IntoIterator<Item = &'a str>) -> Vec<Challenge> {
    let mut out = Vec::new();
    for value in values {
        let mut lx = Lexer { s: value.as_bytes(), i: 0 };
        loop {
            lx.skip_ws_commas();
            if lx.peek().is_none() {
                break;
            }
            let scheme = lx.token();
            if scheme.is_empty() {
                // Garbage; skip a byte to make progress.
                lx.i += 1;
                continue;
            }
            let mut ch = Challenge { scheme: scheme.to_ascii_lowercase(), token68: None, params: Vec::new() };
            lx.skip_ws();
            // token68?
            if lx.peek().is_some() && lx.peek() != Some(b',') && !lx.at_param() {
                let start = lx.i;
                while lx.i < lx.s.len() && lx.s[lx.i] != b',' && lx.s[lx.i] != b' ' {
                    lx.i += 1;
                }
                let t = String::from_utf8_lossy(&lx.s[start..lx.i]).into_owned();
                if !t.is_empty() {
                    ch.token68 = Some(t);
                }
                out.push(ch);
                continue;
            }
            // auth-params until the next scheme.
            loop {
                lx.skip_ws_commas();
                if lx.peek().is_none() || !lx.at_param() {
                    break;
                }
                let name = lx.token().to_ascii_lowercase();
                lx.skip_ws();
                lx.i += 1; // '='
                lx.skip_ws();
                let value = if lx.peek() == Some(b'"') { lx.quoted() } else { lx.token() };
                ch.params.push((name, value));
            }
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_and_basic_in_one_header() {
        let c = parse_challenges([
            r#"Digest realm="http-auth@example.org", qop="auth, auth-int", algorithm=SHA-256, nonce="7ypf/xlj9XXwfDPEoM4URrv/xwf94BcCAzFZH4GiTo0v", opaque="FQhe/qaU925kfnzjCev0ciny7QMkPqMAFRtzCUYo5tdS", Basic realm="x""#,
        ]);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].scheme, "digest");
        assert_eq!(c[0].param("qop"), Some("auth, auth-int"));
        assert_eq!(c[0].param("algorithm"), Some("SHA-256"));
        assert_eq!(c[1].scheme, "basic");
        assert_eq!(c[1].param("realm"), Some("x"));
    }

    #[test]
    fn ntlm_token68() {
        let c = parse_challenges(["NTLM TlRMTVNTUAACAAAADAAMADAAAAA=", "Negotiate"]);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].scheme, "ntlm");
        assert_eq!(c[0].token68.as_deref(), Some("TlRMTVNTUAACAAAADAAMADAAAAA="));
        assert_eq!(c[1].scheme, "negotiate");
        let bare = parse_challenges(["NTLM"]);
        assert_eq!(bare[0].token68, None);
    }

    #[test]
    fn escaped_quotes() {
        let c = parse_challenges([r#"Digest realm="a \"b\" c", nonce=abc"#]);
        assert_eq!(c[0].param("realm"), Some(r#"a "b" c"#));
        assert_eq!(c[0].param("nonce"), Some("abc"));
    }
}
