//! The proxy's own page, served for requests addressed to the proxy itself
//! (or to `http://merpati.proxy/`): setup help and the CA certificate.

use http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use http::{HeaderValue, Method, Request, Response, StatusCode};
use hyper::body::Incoming;

use crate::exchange::{ProxyBody, empty, full};
use crate::server::{Shared, error_response};

pub(crate) fn respond(shared: &Shared, req: &Request<Incoming>) -> Response<ProxyBody> {
    if req.method() != Method::GET && req.method() != Method::HEAD {
        return error_response(
            StatusCode::BAD_REQUEST,
            "this is the Merpati capture proxy; send absolute-form (proxy) requests or CONNECT",
        );
    }
    let head = req.method() == Method::HEAD;
    let ca = shared.cfg.ca.as_ref();
    let (ctype, disposition, body): (&'static str, Option<&'static str>, Vec<u8>) = match req.uri().path() {
        "/" | "/index.html" => ("text/html; charset=utf-8", None, page(shared).into_bytes()),
        "/ca.pem" => match ca {
            Some(ca) => (
                "application/x-pem-file",
                Some("attachment; filename=\"merpati-ca.pem\""),
                ca.cert_pem().as_bytes().to_vec(),
            ),
            None => return error_response(StatusCode::NOT_FOUND, "HTTPS interception is not set up (no CA)"),
        },
        "/ca.crt" | "/ca.der" | "/ca.cer" => match ca {
            Some(ca) => {
                ("application/x-x509-ca-cert", Some("attachment; filename=\"merpati-ca.crt\""), ca.cert_der().to_vec())
            }
            None => return error_response(StatusCode::NOT_FOUND, "HTTPS interception is not set up (no CA)"),
        },
        _ => return error_response(StatusCode::NOT_FOUND, "not found"),
    };
    let len = body.len();
    let mut resp = Response::new(if head { empty() } else { full(body) });
    let h = resp.headers_mut();
    h.insert(CONTENT_TYPE, HeaderValue::from_static(ctype));
    h.insert(http::header::CONTENT_LENGTH, HeaderValue::from(len));
    h.insert(http::header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Some(d) = disposition {
        h.insert(CONTENT_DISPOSITION, HeaderValue::from_static(d));
    }
    resp
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn page(shared: &Shared) -> String {
    let addr = shared.local_addr;
    let ca_html = match shared.cfg.ca.as_ref() {
        Some(ca) => format!(
            "<p>HTTPS is decrypted with the local CA <b>{}</b><br>SHA-256 <code>{}</code></p>\
             <p><a href=\"/ca.pem\">Download CA (PEM)</a> &middot; <a href=\"/ca.crt\">Download CA (DER, for iOS/Android)</a></p>\
             <p>Install and trust it only on devices you own, and remove it when you are done.</p>",
            escape(ca.common_name()),
            ca.fingerprint_sha256()
        ),
        None => "<p>HTTPS interception is off: HTTPS traffic is only tunnelled.</p>".to_owned(),
    };
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\">\
         <title>Merpati capture proxy</title><style>body{{font-family:system-ui,sans-serif;max-width:40rem;margin:2rem auto;\
         padding:0 1rem;line-height:1.5}}code{{word-break:break-all}}</style></head><body>\
         <h1>Merpati capture proxy</h1><p>Listening on <code>{addr}</code>. Configure it as the HTTP and HTTPS proxy of \
         your browser, app or device.</p>{ca_html}</body></html>"
    )
}
