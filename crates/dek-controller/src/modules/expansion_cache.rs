use std::{cell::RefCell, rc::Rc, sync::LazyLock};

use dek_core::HostConfig;
use serde_json::json;

use crate::{
    cache::{
        CacheDependency, CacheDigest, CacheEntry, CacheStore, Publication, UnitKey, UnitStage,
        semantic_digest,
    },
    eval::{DeclarationGraph, RubyProviders, RubySession, observations},
};

use super::{
    ExpansionError, ModuleOutput,
    expansion::RegisteredModule,
    merge_cache::{graph_current, host_scope, merge_cache_key},
    provenance::{Sources, file_origin, map_config_origins},
    repository_modules::ruby_context,
};

pub(super) type RubyRuntime<'a> = Rc<RefCell<(&'a mut RubySession, RubyProviders<'a>)>>;

pub(super) enum ModuleCache<'a> {
    Dynamic,
    Builtin,
    Ruby(RubyRuntime<'a>),
}

impl ModuleCache<'_> {
    fn signature(&self) -> Option<String> {
        match self {
            Self::Dynamic => None,
            Self::Builtin => Some(BUILTIN_SIGNATURE.clone()),
            Self::Ruby(runtime) => runtime
                .borrow()
                .0
                .runtime_signature()
                .map(|signature| format!("expand-v1:{signature}")),
        }
    }

    fn inputs_unchanged(
        &self,
        root: &std::path::Path,
        dependencies: &[CacheDependency],
    ) -> Result<bool, crate::eval::CollectError> {
        match self {
            Self::Ruby(runtime) => observations::unchanged(root, dependencies, |input| {
                runtime.borrow_mut().1.reobserve(input)
            }),
            _ => Ok(dependencies.is_empty()),
        }
    }
}

static BUILTIN_SIGNATURE: LazyLock<String> = LazyLock::new(|| {
    let source = concat!(
        include_str!("builtin_apps/mod.rs"),
        include_str!("builtin_apps/git_config.rs"),
        include_str!("builtin_system/mod.rs"),
        include_str!("builtin_system/accounts.rs"),
        include_str!("builtin_system/units.rs"),
        include_str!("builtin_keys/mod.rs"),
    );
    format!(
        "builtin-expand-v1:{}",
        CacheDigest::of_bytes(source.as_bytes())
    )
});

/// Cache each registered module against the options visible to it for this host.
pub struct ExpansionCache<'a> {
    store: &'a CacheStore,
    graph: &'a DeclarationGraph,
}

impl<'a> ExpansionCache<'a> {
    pub fn new(store: &'a CacheStore, graph: &'a DeclarationGraph) -> Self {
        Self { store, graph }
    }

    pub fn key(&self, module: &str) -> UnitKey {
        UnitKey {
            stage: UnitStage::Expand,
            unit: module.into(),
            params: Default::default(),
            target: Some(host_scope(self.graph)),
        }
    }

    pub(super) fn verify_graph(&self) -> Result<(), ExpansionError> {
        if !graph_current(self.store, self.graph).map_err(expansion_error)? {
            return Err(ExpansionError(format!(
                "declaration inputs for host {:?} changed before expansion; retry",
                self.graph.host.name
            )));
        }
        Ok(())
    }

    pub(super) fn repository_root(&self) -> &std::path::Path {
        self.store.repository_root()
    }

    pub(super) fn expand(
        &self,
        module: &mut RegisteredModule<'_>,
        options: &HostConfig,
    ) -> Result<ModuleOutput, ExpansionError> {
        let root = self.store.repository_root();
        let Some(signature) = module.cache.signature() else {
            return module.evaluate(root, options);
        };
        let key = self.key(&module.definition.name);
        let signature = format!("{signature}:{}", module.definition.origin.file());
        let native = matches!(module.cache, ModuleCache::Builtin);
        let mut sources = Sources::default();
        let normalized = native.then(|| {
            let mut normalized = options.clone();
            for option in normalized.options.values_mut() {
                option.overridden_definitions.clear();
            }
            map_config_origins(&mut normalized, |origin| {
                *origin = sources.reference(origin)
            });
            normalized
        });
        let dependency = CacheDependency::Options {
            paths: module.definition.reads_options.clone(),
            semantic_digest: match normalized.as_ref() {
                Some(normalized) => semantic_digest(&json!(normalized)),
                None => semantic_digest(&ruby_context(options)),
            },
        };

        let merge_key = merge_cache_key(self.graph);
        let prior_merge_revision = self.store.revision(&merge_key).map_err(expansion_error)?;
        let upstream_current = || {
            Ok(self.store.revision(&merge_key)? == prior_merge_revision
                && graph_current(self.store, self.graph)?)
        };
        let stale = || {
            ExpansionError(format!(
                "inputs for module {} changed during expansion; retry",
                key.unit
            ))
        };
        let mut lookup = self
            .store
            .lookup::<ModuleOutput>(&key, &signature)
            .map_err(expansion_error)?;
        if let Some(entry) = lookup.entry.take()
            && let Some((expected, inputs)) = entry.dependencies.split_first()
            && expected == &dependency
            && module.cache.inputs_unchanged(root, inputs).unwrap_or(false)
        {
            if !upstream_current().map_err(expansion_error)? {
                return Err(stale());
            }
            return Ok(restore(entry.output, &sources, native));
        }

        let output = match normalized.as_ref() {
            Some(normalized) => match module.evaluate(root, normalized) {
                Ok(output) => output,
                Err(_) => return module.evaluate(root, options),
            },
            None => module.evaluate(root, options)?,
        };
        output.validate_contents(&module.definition)?;
        let mut dependencies = vec![dependency];
        dependencies.extend(output.inputs.iter().cloned().map(CacheDependency::Input));
        let entry = CacheEntry {
            dependencies,
            semantic_digest: output_digest(&output),
            output,
        };

        let publication = self
            .store
            .publish(&lookup, &entry, |dependencies| {
                if !module.cache.inputs_unchanged(root, &dependencies[1..])? {
                    return Ok(false);
                }
                let current_signature = module
                    .cache
                    .signature()
                    .map(|value| format!("{value}:{}", module.definition.origin.file()));
                Ok(current_signature.as_deref() == Some(signature.as_str()) && upstream_current()?)
            })
            .map_err(expansion_error)?;
        if publication != Publication::Published {
            return Err(stale());
        }
        Ok(restore(entry.output, &sources, native))
    }
}

fn restore(mut output: ModuleOutput, sources: &Sources, native: bool) -> ModuleOutput {
    if native {
        output.map_origins(|origin| sources.resolve(origin));
        for resource in &mut output.resources {
            let mut unique = Vec::new();
            resource.origins.retain(|origin| {
                if unique.contains(origin) {
                    false
                } else {
                    unique.push(origin.clone());
                    true
                }
            });
        }
    }
    output
}

fn output_digest(output: &ModuleOutput) -> CacheDigest {
    let mut output = output.clone();
    output.inputs.clear();
    output.map_origins(file_origin);
    semantic_digest(&json!(output))
}

fn expansion_error(error: crate::cache::CacheError) -> ExpansionError {
    ExpansionError(error.to_string())
}
