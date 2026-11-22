use std::{collections::BTreeMap, path::Path};

use dek_core::{HostConfig, OptionPath, Origin};
use thiserror::Error;

use super::ModuleOutput;

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

struct RegisteredModule<'a> {
    definition: ModuleDefinition,
    expand: Box<Expander<'a>>,
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
            },
        );
        Ok(())
    }

    pub fn expand(
        &mut self,
        repo_root: &Path,
        config: &HostConfig,
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
            let output = (module.expand)(&ExpandContext {
                repo_root,
                options: &options,
            })
            .map_err(|message| {
                ExpansionError(format!(
                    "expand {} at {}: {message}",
                    definition.name, definition.origin
                ))
            })?;
            results.push(ModuleExpansion {
                definition: definition.clone(),
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
