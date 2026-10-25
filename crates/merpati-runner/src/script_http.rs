//! `pm.sendRequest` support: scripts run on blocking threads and call back
//! into the async HTTP engine through the runtime handle.

use merpati_core::auth::Auth;
use merpati_core::model::HttpRequest;
use merpati_core::prepared::{PrepareContext, prepare};
use merpati_core::script::{ScriptHttp, ScriptRequest, ScriptResponse};
use merpati_core::variables::{Resolver, VariableScopes};
use merpati_http::SendOptions;

use crate::pipeline::{Services, script_response};

pub struct PipelineHttp {
    services: Services,
}

impl PipelineHttp {
    pub fn new(services: Services) -> Self {
        PipelineHttp { services }
    }
}

impl ScriptHttp for PipelineHttp {
    fn send(&self, request: ScriptRequest, scopes: &VariableScopes) -> Result<ScriptResponse, String> {
        let mut http = HttpRequest {
            method: if request.method.is_empty() { "GET".into() } else { request.method },
            headers: request.headers,
            body: request.body,
            ..Default::default()
        };
        http.set_url(request.url);
        let mut resolver = Resolver::new(scopes);
        if let Some(v) = &self.services.vault {
            resolver = resolver.with_vault(v.as_ref());
        }
        let auth = Auth::none();
        let ctx = PrepareContext {
            resolver,
            auth: &auth,
            auth_owner_id: None,
            defaults: &self.services.defaults,
            base_dir: None,
        };
        let prepared = prepare(&http, &ctx);
        let engine = self.services.http.clone();
        let fut = async move { engine.send(&prepared, &SendOptions::default()).await };
        // Called from a blocking (non-runtime) thread, so block_on is allowed.
        let out = self.services.runtime.block_on(fut).map_err(|e| e.message)?;
        Ok(script_response(&out.response))
    }
}
