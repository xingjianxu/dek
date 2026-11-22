use std::{collections::BTreeMap, path::Path};

use dek_core::{HostConfig, OptionPath, Origin};
use thiserror::Error;

use super::{ExpansionCache, ModuleOutput, expansion_cache::ModuleCache};

#[derive(Clone, Debug)]
pub struct ModuleDefinition {
    pub name: String,
    pub origin: Origin,
    /// Exact paths or subtree prefixes. An omitted list exposes all options.
    pub reads_options: Option<Vec<OptionPath>>,
    pub enabled_by: Option<OptionPath>,
}

pub struct ExpandContext<'a> {
    pub repo_root: &'a Path,
    pub options: &'a HostConfig,
}

#[derive(Clone, Debug)]
pub struct ModuleExpansion {
    pub definition: ModuleDefinition,
    pub options: HostConfig,
    pub output: ModuleOutput,
}

#[derive(Debug, Error)]
#[error("{0}")]
pub struct ExpansionError(pub(crate) String);

type Expander<'a> = dyn FnMut(&ExpandContext<'_>) -> Result<ModuleOutput, String> + 'a;

pub(super) struct RegisteredModule<'a> {
    pub(super) definition: ModuleDefinition,
    expand: Box<Expander<'a>>,
    pub(super) cache: ModuleCache<'a>,
}

impl RegisteredModule<'_> {
    pub(super) fn evaluate(
        &mut self,
        repo_root: &Path,
        options: &HostConfig,
    ) -> Result<ModuleOutput, ExpansionError> {
        (self.expand)(&ExpandContext { repo_root, options }).map_err(|message| {
            ExpansionError(format!(
                "expand {} at {}: {message}",
                self.definition.name, self.definition.origin
            ))
        })
    }
}

#[derive(Default)]
pub struct ModuleRegistry<'a> {
    modules: BTreeMap<String, RegisteredModule<'a>>,
}

impl<'a> ModuleRegistry<'a> {
    pub fn register(
        &mut self,
        definition: ModuleDefinition,
        expand: impl FnMut(&ExpandContext<'_>) -> Result<ModuleOutput, String> + 'a,
    ) -> Result<(), ExpansionError> {
        self.register_with_cache(definition, expand, ModuleCache::Dynamic)
    }

    pub(super) fn register_builtin(
        &mut self,
        definition: ModuleDefinition,
        expand: impl FnMut(&ExpandContext<'_>) -> Result<ModuleOutput, String> + 'a,
    ) -> Result<(), ExpansionError> {
        self.register_with_cache(definition, expand, ModuleCache::Builtin)
    }

    pub(super) fn register_with_cache(
        &mut self,
        definition: ModuleDefinition,
        expand: impl FnMut(&ExpandContext<'_>) -> Result<ModuleOutput, String> + 'a,
        cache: ModuleCache<'a>,
    ) -> Result<(), ExpansionError> {
        if let Some(prior) = self.modules.get(&definition.name) {
            return Err(ExpansionError(format!(
                "duplicate module {} at {} and {}",
                definition.name, prior.definition.origin, definition.origin
            )));
        }
        self.modules.insert(
            definition.name.clone(),
            RegisteredModule {
                definition,
                expand: Box::new(expand),
                cache,
            },
        );
        Ok(())
    }

    pub fn expand(
        &mut self,
        repo_root: &Path,
        config: &HostConfig,
    ) -> Result<Vec<ModuleExpansion>, ExpansionError> {
        self.expand_inner(repo_root, config, None)
    }

    /// Reuse eligible module outputs. Deferred files remain for resource assembly.
    pub fn expand_cached(
        &mut self,
        cache: &ExpansionCache<'_>,
        config: &HostConfig,
    ) -> Result<Vec<ModuleExpansion>, ExpansionError> {
        cache.verify_graph()?;
        self.expand_inner(cache.repository_root(), config, Some(cache))
    }

    fn expand_inner(
        &mut self,
        repo_root: &Path,
        config: &HostConfig,
        cache: Option<&ExpansionCache<'_>>,
    ) -> Result<Vec<ModuleExpansion>, ExpansionError> {
        let mut results = Vec::new();
        for module in self.modules.values_mut() {
            let definition = &module.definition;
            if let Some(enabled_by) = &definition.enabled_by
                && config
                    .options
                    .get(enabled_by)
                    .and_then(|option| option.value.as_bool())
                    != Some(true)
            {
                continue;
            }

            let options = HostConfig {
                options: config
                    .options
                    .iter()
                    .filter(|(path, _)| definition.uses(path))
                    .map(|(path, option)| (path.clone(), option.clone()))
                    .collect(),
            };
            let output = match cache {
                Some(cache) => cache.expand(module, &options)?,
                None => module.evaluate(repo_root, &options)?,
            };
            results.push(ModuleExpansion {
                definition: module.definition.clone(),
                options,
                output,
            });
        }
        Ok(results)
    }
}

impl ModuleDefinition {
    fn uses(&self, path: &OptionPath) -> bool {
        self.reads_options.as_ref().is_none_or(|prefixes| {
            prefixes.iter().any(|prefix| {
                path == prefix
                    || path
                        .as_str()
                        .strip_prefix(prefix.as_str())
                        .is_some_and(|suffix| suffix.starts_with('.'))
            })
        })
    }
}
