//! Host functions exposed to the prelude (all prefixed `__mp_host_`). They
//! are the only way scripts can reach outside the sandbox.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use boa_engine::error::PanicError;
use boa_engine::object::builtins::JsArray;
use boa_engine::{Context, JsError, JsNativeError, JsResult, JsString, JsValue, NativeFunction, Script, Source};
use merpati_core::response::ResponseBody;
use merpati_core::script::{PackageContext, ScriptHttp, ScriptRequest};
use merpati_core::variables::VariableScopes;
use rand::RngCore;

use crate::packages::{PackageLimits, Session};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Interrupt {
    Timeout,
    Cancelled,
}

/// Per-job data reachable from host functions through `Context` data.
pub(crate) struct JobData {
    pub deadline: Instant,
    pub timeout_ms: u128,
    pub cancel: Arc<AtomicBool>,
    pub response_body: Option<ResponseBody>,
    pub http: Option<Arc<dyn ScriptHttp>>,
    pub interrupted: Cell<Option<Interrupt>>,
    pub packages: Option<Arc<PackageContext>>,
    pub package_limits: PackageLimits,
    /// Created on the first package lookup of the job.
    pub pkg_session: RefCell<Option<Rc<RefCell<Session>>>>,
}

impl JobData {
    pub(crate) fn interrupt_now(&self) -> Option<Interrupt> {
        if let Some(i) = self.interrupted.get() {
            return Some(i);
        }
        let i = if self.cancel.load(Ordering::Relaxed) {
            Interrupt::Cancelled
        } else if Instant::now() >= self.deadline {
            Interrupt::Timeout
        } else {
            return None;
        };
        self.interrupted.set(Some(i));
        Some(i)
    }

    pub(crate) fn package_session(&self) -> Result<Rc<RefCell<Session>>, String> {
        if let Some(s) = self.pkg_session.borrow().as_ref() {
            return Ok(s.clone());
        }
        let Some(ctx) = self.packages.clone() else {
            return Err("npm/JSR packages and the Package Library are not available here".into());
        };
        let s = Rc::new(RefCell::new(Session::new(ctx, self.package_limits, self.deadline, self.cancel.clone())));
        *self.pkg_session.borrow_mut() = Some(s.clone());
        Ok(s)
    }

    pub(crate) fn interrupt_message(&self, i: Interrupt) -> String {
        match i {
            Interrupt::Timeout => format!("Script execution timed out after {} ms", self.timeout_ms),
            Interrupt::Cancelled => "Script execution was cancelled".to_string(),
        }
    }
}

/// Throws an uncatchable error when the job ran out of time or was cancelled.
pub(crate) fn check_deadline(ctx: &mut Context) -> JsResult<()> {
    let Some(data) = ctx.get_data::<JobData>() else { return Ok(()) };
    match data.interrupt_now() {
        Some(i) => Err(PanicError::new(data.interrupt_message(i)).into()),
        None => Ok(()),
    }
}

fn arg_string(args: &[JsValue], i: usize, ctx: &mut Context) -> JsResult<String> {
    match args.get(i) {
        Some(v) if !v.is_undefined() => Ok(v.to_string(ctx)?.to_std_string_escaped()),
        _ => Ok(String::new()),
    }
}

fn type_error(msg: impl Into<String>) -> JsError {
    JsNativeError::typ().with_message(msg.into()).into()
}

fn host_check(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    check_deadline(ctx)?;
    Ok(JsValue::undefined())
}

fn response_bytes(ctx: &mut Context) -> JsResult<bytes::Bytes> {
    check_deadline(ctx)?;
    let Some(body) = ctx.get_data::<JobData>().and_then(|d| d.response_body.clone()) else {
        return Ok(bytes::Bytes::new());
    };
    body.read_all().map_err(|e| type_error(format!("could not read response body: {e}")))
}

fn host_body(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let bytes = response_bytes(ctx)?;
    Ok(JsString::from(String::from_utf8_lossy(&bytes).as_ref()).into())
}

fn host_body_base64(_: &JsValue, _: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    use base64::Engine;
    let bytes = response_bytes(ctx)?;
    Ok(JsString::from(base64::engine::general_purpose::STANDARD.encode(&bytes)).into())
}

fn host_dynamic(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = arg_string(args, 0, ctx)?;
    Ok(match merpati_core::dynamic::generate(&name) {
        Some(v) => JsString::from(v).into(),
        None => JsValue::undefined(),
    })
}

fn host_random(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let n = args.first().map(|v| v.to_u32(ctx)).transpose()?.unwrap_or(16).min(65_536) as usize;
    let mut buf = vec![0u8; n];
    rand::rng().fill_bytes(&mut buf);
    Ok(JsArray::from_iter(buf.into_iter().map(|b| JsValue::from(i32::from(b))), ctx).into())
}

/// `__mp_host_send(requestJson, scopesJson)` -> JSON `{ok, response | error}`.
fn host_send(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    check_deadline(ctx)?;
    let req_json = arg_string(args, 0, ctx)?;
    let scopes_json = arg_string(args, 1, ctx)?;
    let request: ScriptRequest =
        serde_json::from_str(&req_json).map_err(|e| type_error(format!("invalid request for pm.sendRequest: {e}")))?;
    let scopes: VariableScopes = serde_json::from_str(&scopes_json).unwrap_or_default();
    let Some(http) = ctx.get_data::<JobData>().and_then(|d| d.http.clone()) else {
        let out = serde_json::json!({"ok": false, "error": "pm.sendRequest is not available in this context"});
        return Ok(JsString::from(out.to_string()).into());
    };
    let result = http.send(request, &scopes);
    // The request may have taken a while; honour the deadline before
    // handing control back to the script.
    check_deadline(ctx)?;
    let out = match result {
        Ok(resp) => {
            let body = resp.body.read_all().map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
            serde_json::json!({
                "ok": true,
                "response": {
                    "code": resp.code,
                    "status": resp.status,
                    "headers": resp.headers,
                    "body": body,
                    "responseTime": resp.response_time_ms,
                    "responseSize": resp.response_size,
                }
            })
        }
        Err(e) => serde_json::json!({"ok": false, "error": e}),
    };
    Ok(JsString::from(out.to_string()).into())
}

const CHAI: &str = include_str!("../js/vendor/chai.js");
const LODASH: &str = include_str!("../js/vendor/lodash.min.js");
const MOMENT: &str = include_str!("../js/vendor/moment.min.js");
const CRYPTO_JS: &str = include_str!("../js/vendor/crypto-js.js");
const TV4: &str = include_str!("../js/vendor/tv4.js");
// Polyfills for what boa is built without (no ICU, to keep the binary small).
const INTL: &str = include_str!("../js/intl.js");
const STREAMS: &str = include_str!("../js/streams.js");

/// Source of a bundled library by module name.
pub(crate) fn vendored(name: &str) -> Option<&'static str> {
    Some(match name {
        "chai" => CHAI,
        "lodash" => LODASH,
        "moment" => MOMENT,
        "crypto-js" => CRYPTO_JS,
        "tv4" => TV4,
        "intl" => INTL,
        "streams" => STREAMS,
        _ => return None,
    })
}

/// Evaluates a bundled CommonJS/UMD library and returns `module.exports`.
fn host_load(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = arg_string(args, 0, ctx)?;
    let Some(src) = vendored(&name) else {
        return Err(type_error(format!("unknown bundled module '{name}'")));
    };
    let wrapped = format!(
        "(function () {{ var module = {{ exports: {{}} }}; var exports = module.exports; var define = undefined; \
         var require = function (m) {{ throw new Error(\"Cannot find module '\" + m + \"'\"); }};\n{src}\n;\
         return module.exports; }}).call(globalThis)"
    );
    let script = Script::parse(Source::from_bytes(wrapped.as_bytes()), None, ctx)?;
    script.evaluate(ctx)
}

/// `__mp_host_tz_local()` -> the local IANA time zone name, or "".
fn host_tz_local(_: &JsValue, _: &[JsValue], _: &mut Context) -> JsResult<JsValue> {
    Ok(JsString::from(crate::tz::local_name().unwrap_or_default()).into())
}

/// `__mp_host_tz_offset(name, epochMs)` -> `[offsetSeconds, abbreviation,
/// canonicalName, isDst]`, or undefined for zones the system does not know.
fn host_tz_offset(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    let name = arg_string(args, 0, ctx)?;
    let ms = args.get(1).map(|v| v.to_number(ctx)).transpose()?.unwrap_or(0.0);
    if !ms.is_finite() {
        return Ok(JsValue::undefined());
    }
    let Some(t) = crate::tz::offset_at(&name, (ms / 1000.0).floor() as i64) else {
        return Ok(JsValue::undefined());
    };
    let items =
        [JsValue::from(t.offset), JsString::from(t.abbr).into(), JsString::from(t.name).into(), t.is_dst.into()];
    Ok(JsArray::from_iter(items, ctx).into())
}

type HostFn = fn(&JsValue, &[JsValue], &mut Context) -> JsResult<JsValue>;

pub(crate) fn register(ctx: &mut Context) -> JsResult<()> {
    let fns: [(&str, usize, HostFn); 11] = [
        ("__mp_host_check", 0, host_check),
        ("__mp_host_body", 0, host_body),
        ("__mp_host_body_base64", 0, host_body_base64),
        ("__mp_host_dynamic", 1, host_dynamic),
        ("__mp_host_random", 1, host_random),
        ("__mp_host_send", 2, host_send),
        ("__mp_host_load", 1, host_load),
        ("__mp_host_pkg_resolve", 2, crate::packages::host_pkg_resolve),
        ("__mp_host_pkg_load", 3, crate::packages::host_pkg_load),
        ("__mp_host_tz_local", 0, host_tz_local),
        ("__mp_host_tz_offset", 2, host_tz_offset),
    ];
    for (name, len, f) in fns {
        ctx.register_global_builtin_callable(JsString::from(name), len, NativeFunction::from_fn_ptr(f))?;
    }
    Ok(())
}
