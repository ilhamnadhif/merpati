//! Packages for `pm.require` besides the bundled libraries: `npm:` and
//! `jsr:` specifiers loaded as ES modules from an esm.sh-compatible CDN, and
//! the local Package Library (`<workspace>/packages/`).
//!
//! Modules are fetched host-side ([`fetch`]), cached and pinned in the
//! lockfile ([`store`]), and served to boa by [`PackageLoader`]. Remote
//! modules stay parsed for the lifetime of a worker; local ones are loaded
//! fresh for every job so edits show up immediately.

mod fetch;
pub(crate) mod spec;
pub(crate) mod store;

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use boa_engine::builtins::promise::PromiseState;
use boa_engine::module::{ModuleLoader, ModuleRequest, Referrer, SyntheticModuleInitializer};
use boa_engine::{Context, JsError, JsNativeError, JsObject, JsResult, JsString, JsValue, Module, Script, Source};
use serde_json::json;

use crate::executor::Executor;
use crate::host::{JobData, check_deadline};
use spec::LocalKind;
pub(crate) use store::{PackageLimits, Session};

/// Worker-wide objects host functions need, stored in the context data.
pub(crate) struct WorkerShared {
    pub executor: Rc<Executor>,
    pub loader: Rc<PackageLoader>,
}

struct RemoteModule {
    module: Module,
    integrity: String,
    /// Requested URLs of direct imports.
    deps: Vec<String>,
}

/// Serves package modules to boa.
#[derive(Default)]
pub(crate) struct PackageLoader {
    /// Remote modules by requested URL; all of them were evaluated.
    remote: RefCell<HashMap<String, RemoteModule>>,
    /// Final URL (module path) -> requested URL.
    paths: RefCell<HashMap<String, String>>,
    /// Local ES modules of the current job.
    local: RefCell<HashMap<PathBuf, Module>>,
    /// Remote modules first parsed by the running top-level load.
    fresh: RefCell<Vec<String>>,
    /// `__mp.importValue(spec, parent)`: built-in and CommonJS modules for
    /// `import` statements in Package Library ES modules.
    import_hook: RefCell<Option<JsObject>>,
}

fn err(msg: impl Into<String>) -> JsError {
    JsNativeError::error().with_message(msg.into()).into()
}

fn session_of(ctx: &Context) -> Result<Rc<RefCell<Session>>, String> {
    match ctx.get_data::<JobData>() {
        Some(d) => d.package_session(),
        None => Err("packages are not available outside a script job".into()),
    }
}

fn js_message(e: JsError, ctx: &mut Context) -> String {
    match e.try_native(ctx) {
        Ok(n) => n.message().to_string(),
        Err(_) => e.to_string(),
    }
}

enum Resolved {
    Remote(String),
    Local(PathBuf, LocalKind),
}

fn not_found(spec: &str, session: &Session) -> String {
    match session.library_dir() {
        Some(dir) => format!("Cannot find module '{spec}' (looked in the package library {})", dir.display()),
        None => format!("Cannot find module '{spec}'"),
    }
}

/// Resolves a specifier given to `pm.require` (from a script, or from a
/// CommonJS package file `parent`).
fn resolve(session: &mut Session, spec: &str, parent: Option<&Path>) -> Result<Resolved, String> {
    if spec.starts_with("npm:") || spec.starts_with("jsr:") {
        let url = spec::remote_entry(session.registry()?, spec).unwrap_or_else(|| Err(not_found(spec, session)))?;
        return Ok(Resolved::Remote(url));
    }
    let Some(lib) = session.library_dir().cloned() else { return Err(not_found(spec, session)) };
    match spec::local_file(&lib, spec, parent)? {
        Some((path, guess)) => {
            let source = read_local(&path)?;
            Ok(Resolved::Local(path.clone(), spec::sniff_kind(&path, &source, guess)))
        }
        None => Err(not_found(spec, session)),
    }
}

fn read_local(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("could not read {}: {e}", path.display()))
}

impl PackageLoader {
    pub(crate) fn set_import_hook(&self, hook: JsObject) {
        *self.import_hook.borrow_mut() = Some(hook);
    }

    /// Forgets per-job state (local modules).
    pub(crate) fn end_job(&self) {
        self.local.borrow_mut().clear();
        self.fresh.borrow_mut().clear();
    }

    /// Checks the pins of a cached module and everything it imports.
    fn ensure_pinned(&self, url: &str, session: &Rc<RefCell<Session>>) -> Result<(), String> {
        let mut stack = vec![url.to_string()];
        let remote = self.remote.borrow();
        let mut s = session.borrow_mut();
        while let Some(u) = stack.pop() {
            if !s.verified.insert(u.clone()) {
                continue;
            }
            if let Some(m) = remote.get(&u) {
                s.check_pin(&u, &m.integrity)?;
                stack.extend(m.deps.iter().cloned());
            }
        }
        Ok(())
    }

    fn remote_module(&self, url: &str, session: &Rc<RefCell<Session>>, ctx: &mut Context) -> JsResult<Module> {
        let cached = self.remote.borrow().get(url).map(|m| m.module.clone());
        if let Some(m) = cached {
            self.ensure_pinned(url, session).map_err(err)?;
            return Ok(m);
        }
        let got = session.borrow_mut().obtain(url).map_err(err)?;
        let source = Source::from_bytes(got.source.as_bytes()).with_path(Path::new(&got.final_url));
        let module = Module::parse(source, None, ctx).map_err(|e| {
            session.borrow_mut().evict(url);
            let msg = js_message(e, ctx);
            JsError::from(JsNativeError::syntax().with_message(format!("{url}: {msg}")))
        })?;
        self.paths.borrow_mut().insert(got.final_url.clone(), url.to_string());
        self.remote.borrow_mut().insert(
            url.to_string(),
            RemoteModule { module: module.clone(), integrity: got.integrity, deps: Vec::new() },
        );
        self.fresh.borrow_mut().push(url.to_string());
        Ok(module)
    }

    fn local_module(&self, path: &Path, ctx: &mut Context) -> JsResult<Module> {
        if let Some(m) = self.local.borrow().get(path) {
            return Ok(m.clone());
        }
        let text = read_local(path).map_err(err)?;
        let module = Module::parse(Source::from_bytes(text.as_bytes()).with_path(path), None, ctx)?;
        self.local.borrow_mut().insert(path.to_path_buf(), module.clone());
        Ok(module)
    }

    /// A synthetic module for a value produced by `require` (built-ins and
    /// CommonJS files imported from ES modules).
    fn value_module(&self, spec: &str, parent: &str, ctx: &mut Context) -> JsResult<Module> {
        let hook = self.import_hook.borrow().clone().ok_or_else(|| err("module imports are not set up"))?;
        let out =
            hook.call(&JsValue::undefined(), &[JsString::from(spec).into(), JsString::from(parent).into()], ctx)?;
        let out = out.as_object().ok_or_else(|| err("bad import hook result"))?;
        let value = out.get(JsString::from("value"), ctx)?;
        let names_val = out.get(JsString::from("names"), ctx)?;
        let mut names = vec![JsString::from("default")];
        if let Some(arr) = names_val.as_object() {
            let len = arr.get(JsString::from("length"), ctx)?.to_length(ctx)?;
            for i in 0..len {
                let n = arr.get(i, ctx)?.to_string(ctx)?;
                names.push(n);
            }
        }
        let init = SyntheticModuleInitializer::from_copy_closure_with_captures(
            |m, (value, names), ctx| {
                m.set_export(&names[0], value.clone())?;
                if let Some(o) = value.as_object() {
                    for n in &names[1..] {
                        m.set_export(n, o.get(n.clone(), ctx)?)?;
                    }
                }
                Ok(())
            },
            (value, names.clone()),
        );
        Ok(Module::synthetic(&names, init, None, None, ctx))
    }

    /// Handles one `import` of a module being loaded.
    fn import(&self, specifier: &str, from: Option<&str>, ctx: &mut Context) -> JsResult<Module> {
        let session = session_of(ctx).map_err(err)?;
        let from_remote = from.filter(|f| f.starts_with("https://") || f.starts_with("http://"));
        if let Some(referrer) = from_remote {
            let url = {
                let s = session.borrow();
                let registry = s.registry().map_err(err)?;
                spec::remote_import(registry, specifier, referrer).map_err(err)?
            };
            let module = self.remote_module(&url, &session, ctx)?;
            let key = self.paths.borrow().get(referrer).cloned();
            if let Some(key) = key
                && let Some(m) = self.remote.borrow_mut().get_mut(&key)
                && !m.deps.contains(&url)
            {
                m.deps.push(url);
            }
            return Ok(module);
        }
        let parent = from.map(Path::new).filter(|p| p.is_absolute());
        let resolved = {
            let mut s = session.borrow_mut();
            let local_parent_import =
                parent.is_some() && !specifier.starts_with("npm:") && !specifier.starts_with("jsr:");
            match resolve(&mut s, specifier, parent) {
                Ok(r) => Ok(r),
                // Not a library file: let `require` find built-ins.
                Err(_) if local_parent_import && !specifier.starts_with('.') => {
                    drop(s);
                    return self.value_module(specifier, from.unwrap_or_default(), ctx);
                }
                Err(e) => Err(e),
            }
        };
        match resolved.map_err(err)? {
            Resolved::Remote(url) => self.remote_module(&url, &session, ctx),
            Resolved::Local(path, LocalKind::Esm) => self.local_module(&path, ctx),
            Resolved::Local(_, LocalKind::Cjs) => self.value_module(specifier, from.unwrap_or_default(), ctx),
        }
    }

    /// Loads, links and evaluates a module graph synchronously.
    fn run_graph(&self, entry: &Module, ctx: &mut Context) -> JsResult<JsObject> {
        let executor = ctx.get_data::<WorkerShared>().map(|w| w.executor.clone()).ok_or_else(|| err("no executor"))?;
        let promise = entry.load(ctx);
        executor.run_async_jobs(ctx)?;
        match promise.state() {
            PromiseState::Fulfilled(_) => {}
            PromiseState::Rejected(e) => return Err(JsError::from_opaque(e)),
            PromiseState::Pending => return Err(err("the module graph did not finish loading")),
        }
        entry.link(ctx)?;
        let done = entry.evaluate(ctx)?;
        if matches!(done.state(), PromiseState::Pending) {
            // Top-level await: settle it before returning to the script.
            ctx.run_jobs()?;
        }
        match done.state() {
            PromiseState::Fulfilled(_) => Ok(entry.namespace(ctx)),
            PromiseState::Rejected(e) => Err(JsError::from_opaque(e)),
            PromiseState::Pending => Err(err("the package uses a top-level await that never settled")),
        }
    }

    /// Loads a package entry and its imports. Remote modules first parsed by
    /// a load that fails are forgotten, along with their new pins.
    fn load_entry(
        &self,
        ctx: &mut Context,
        entry: impl FnOnce(&Self, &Rc<RefCell<Session>>, &mut Context) -> JsResult<Module>,
    ) -> JsResult<JsObject> {
        let session = session_of(ctx).map_err(err)?;
        let outer_fresh = std::mem::take(&mut *self.fresh.borrow_mut());
        let outer = session.borrow_mut().begin_graph();
        let result = entry(self, &session, ctx).and_then(|m| self.run_graph(&m, ctx));
        let fresh = std::mem::replace(&mut *self.fresh.borrow_mut(), outer_fresh);
        session.borrow_mut().end_graph(outer, result.is_err());
        if result.is_err() {
            let mut remote = self.remote.borrow_mut();
            for u in &fresh {
                remote.remove(u);
            }
            self.paths.borrow_mut().retain(|_, v| !fresh.contains(v));
        } else if let Err(e) = session.borrow_mut().flush() {
            tracing::warn!("{e}");
        }
        result
    }

    fn load_remote(&self, url: &str, ctx: &mut Context) -> JsResult<JsObject> {
        let cached = self.remote.borrow().get(url).map(|m| m.module.clone());
        if let Some(m) = cached {
            let session = session_of(ctx).map_err(err)?;
            self.ensure_pinned(url, &session).map_err(err)?;
            return Ok(m.namespace(ctx));
        }
        self.load_entry(ctx, |me, session, ctx| me.remote_module(url, session, ctx))
    }

    fn load_local(&self, path: &Path, ctx: &mut Context) -> JsResult<JsObject> {
        self.load_entry(ctx, |me, _, ctx| me.local_module(path, ctx))
    }
}

impl ModuleLoader for PackageLoader {
    fn load_imported_module(
        self: Rc<Self>,
        referrer: Referrer,
        request: ModuleRequest,
        context: &RefCell<&mut Context>,
    ) -> impl Future<Output = JsResult<Module>> {
        let specifier = request.specifier().to_std_string_escaped();
        let from = referrer.path().map(|p| p.to_string_lossy().into_owned());
        let result = {
            let mut guard = context.borrow_mut();
            let ctx: &mut Context = &mut guard;
            check_deadline(ctx).and_then(|_| self.import(&specifier, from.as_deref(), ctx))
        };
        std::future::ready(result)
    }
}

fn arg(args: &[JsValue], i: usize, ctx: &mut Context) -> JsResult<String> {
    match args.get(i) {
        Some(v) if !v.is_undefined() && !v.is_null() => Ok(v.to_string(ctx)?.to_std_string_escaped()),
        _ => Ok(String::new()),
    }
}

fn shared(ctx: &Context) -> JsResult<Rc<PackageLoader>> {
    ctx.get_data::<WorkerShared>().map(|w| w.loader.clone()).ok_or_else(|| err("packages are not set up"))
}

/// `__mp_host_pkg_resolve(spec, parent)` -> JSON `{id, kind}` with `kind`
/// `remote`, `esm` or `cjs`. Cached remote graphs are re-checked against
/// the lockfile here, once per job.
pub(crate) fn host_pkg_resolve(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    check_deadline(ctx)?;
    let spec = arg(args, 0, ctx)?;
    let parent = arg(args, 1, ctx)?;
    let parent = (!parent.is_empty()).then(|| PathBuf::from(&parent));
    let loader = shared(ctx)?;
    let session = session_of(ctx).map_err(err)?;
    let resolved = resolve(&mut session.borrow_mut(), &spec, parent.as_deref()).map_err(err)?;
    let out = match resolved {
        Resolved::Remote(url) => {
            if loader.remote.borrow().contains_key(&url) {
                loader.ensure_pinned(&url, &session).map_err(|e| err(format!("Cannot load '{spec}': {e}")))?;
                session.borrow_mut().note_package(&spec, &url);
            }
            json!({"id": url, "kind": "remote"})
        }
        Resolved::Local(path, kind) => json!({
            "id": path.to_string_lossy(),
            "kind": if kind == LocalKind::Esm { "esm" } else { "cjs" },
        }),
    };
    Ok(JsString::from(out.to_string()).into())
}

/// `__mp_host_pkg_load(id, kind, spec)`: the module namespace for `remote`
/// and `esm`, or the compiled CommonJS wrapper
/// `function (exports, require, module, __filename, __dirname)` for `cjs`.
pub(crate) fn host_pkg_load(_: &JsValue, args: &[JsValue], ctx: &mut Context) -> JsResult<JsValue> {
    check_deadline(ctx)?;
    let id = arg(args, 0, ctx)?;
    let kind = arg(args, 1, ctx)?;
    let spec = arg(args, 2, ctx)?;
    let loader = shared(ctx)?;
    let wrap = |e: JsError, ctx: &mut Context| -> JsError {
        if let Ok(native) = e.try_native(ctx)
            && matches!(native.kind(), boa_engine::error::JsNativeErrorKind::Error)
        {
            return err(format!("Cannot load '{spec}': {}", native.message()));
        }
        e
    };
    match kind.as_str() {
        "remote" => {
            let ns = loader.load_remote(&id, ctx).map_err(|e| wrap(e, ctx))?;
            if let Ok(session) = session_of(ctx) {
                let mut s = session.borrow_mut();
                s.note_package(&spec, &id);
                if let Err(e) = s.flush() {
                    tracing::warn!("{e}");
                }
            }
            Ok(ns.into())
        }
        "esm" => loader.load_local(Path::new(&id), ctx).map(JsValue::from).map_err(|e| wrap(e, ctx)),
        _ => {
            let path = PathBuf::from(&id);
            let text = read_local(&path).map_err(err)?;
            let wrapped = format!("(function (exports, require, module, __filename, __dirname) {{{text}\n}})");
            let script = Script::parse(Source::from_bytes(wrapped.as_bytes()).with_path(&path), None, ctx)?;
            script.evaluate(ctx)
        }
    }
}
