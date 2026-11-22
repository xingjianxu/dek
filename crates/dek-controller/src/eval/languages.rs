use std::path::Path;

use dek_core::{UnitSpec, protocol::EvalOperation};
use serde_json::json;

use super::{
    CollectError, CollectRequest, Collector, HttpInputProvider, JsonCollector, RubyProviders,
    RubySession, TargetProvider,
};

/// 使用同一个 Ruby 会话采集 JSON 和 Ruby 混合仓库。
pub struct LanguageCollector<'a> {
    ruby: &'a mut RubySession,
    providers: RubyProviders<'a>,
}

impl<'a> LanguageCollector<'a> {
    pub fn new(ruby: &'a mut RubySession, target: &'a mut dyn TargetProvider) -> Self {
        Self {
            ruby,
            providers: RubyProviders { target, http: None },
        }
    }

    pub fn with_http_provider(mut self, http: &'a mut dyn HttpInputProvider) -> Self {
        self.providers.http = Some(http);
        self
    }
}

impl Collector for LanguageCollector<'_> {
    fn collect(&mut self, request: &CollectRequest<'_>) -> Result<UnitSpec, CollectError> {
        match Path::new(request.entry)
            .extension()
            .and_then(|ext| ext.to_str())
        {
            Some("json") => return JsonCollector.collect(request),
            Some("rb") => {}
            _ => {
                return Err(CollectError {
                    origin: None,
                    message: "入口需要 .json 或 .rb 扩展名".into(),
                });
            }
        }
        let output = self
            .ruby
            .evaluate_with_sdk(
                EvalOperation::Collect,
                request.repo_root,
                &request.entry_path(),
                json!({ "params": request.params }),
                self.providers.reborrow(),
            )
            .map_err(|error| CollectError {
                origin: error.origin,
                message: format!("Ruby collect: {}", error.message),
            })?;
        let mut spec: UnitSpec = serde_json::from_value(output.result).map_err(|err| {
            self.ruby.invalidate();
            CollectError {
                origin: None,
                message: format!("Ruby collect returned invalid declarations: {err}"),
            }
        })?;
        spec.inputs.extend(output.inputs);
        Ok(spec)
    }
}
