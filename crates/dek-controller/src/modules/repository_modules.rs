use std::{cell::RefCell, collections::BTreeMap, fs, num::NonZeroU32, path::PathBuf, rc::Rc};

use dek_core::{HostConfig, OptionPath, Origin, protocol::EvalOperation};
use serde_json::{Value, json};
use thiserror::Error;

use crate::{
    eval::{RubyProviders, RubySession},
    repo::{InputPolicy, Repository},
};

use super::{
    ExpansionError, ModuleDefinition, ModuleOutput, ModuleRegistry, SchemaError, SchemaRegistry,
    expansion_cache::ModuleCache,
};

#[derive(Debug, Error)]
pub enum RepositoryModuleError {
    #[error(transparent)]
    Schema(#[from] SchemaError),
    #[error("cannot read schema for module {module} at {path}: {source}")]
    SchemaIo {
        module: String,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid option path {path:?} for module {module} in dek.json: {source}")]
    OptionPath {
        module: String,
        path: String,
        #[source]
        source: dek_core::InvalidOptionPath,
    },
}

#[derive(Debug)]
pub struct RepositoryModules {
    repo_root: PathBuf,
    entries: Vec<RepositoryModule>,
}

#[derive(Debug)]
struct RepositoryModule {
    definition: ModuleDefinition,
    entry: PathBuf,
    inputs: Option<InputPolicy>,
}

impl RepositoryModules {
    /// Load declarations for repository modules into the shared static registry.
    pub fn load(
        repo: &Repository,
        schemas: &mut SchemaRegistry,
    ) -> Result<Self, RepositoryModuleError> {
        let original_count = schemas.options.len();
        let result = Self::load_into(repo, schemas);
        if result.is_err() {
            schemas.options.truncate(original_count);
        }
        result
    }

    fn load_into(
        repo: &Repository,
        schemas: &mut SchemaRegistry,
    ) -> Result<Self, RepositoryModuleError> {
        let mut entries = Vec::new();
        for (name, module) in &repo.manifest().modules {
            let option_path = |path: &str| {
                path.parse::<OptionPath>()
                    .map_err(|source| RepositoryModuleError::OptionPath {
                        module: name.clone(),
                        path: path.into(),
                        source,
                    })
            };
            let reads_options = module
                .reads_options
                .as_ref()
                .map(|paths| {
                    paths
                        .iter()
                        .map(|path| option_path(path))
                        .collect::<Result<Vec<_>, _>>()
                })
                .transpose()?;
            let enabled_by = Some(option_path(&format!("{name}.enable"))?);
            let schema_path = repo.root().join(&module.schema);
            let schema = fs::read_to_string(&schema_path).map_err(|source| {
                RepositoryModuleError::SchemaIo {
                    module: name.clone(),
                    path: schema_path,
                    source,
                }
            })?;
            schemas.register_json(name, &module.schema.to_string_lossy(), &schema)?;
            let definition = ModuleDefinition {
                name: name.clone(),
                origin: Origin::Ruby {
                    file: module.expand.to_string_lossy().into_owned(),
                    line: NonZeroU32::MIN,
                },
                reads_options,
                enabled_by,
            };
            entries.push(RepositoryModule {
                definition,
                entry: module.expand.clone(),
                inputs: module.inputs,
            });
        }
        Ok(Self {
            repo_root: repo.root().to_owned(),
            entries,
        })
    }

    pub fn input_policy(&self, name: &str) -> Option<InputPolicy> {
        self.entries
            .iter()
            .find(|module| module.definition.name == name)?
            .inputs
    }

    /// Attach expanders to a shared session after declaration collection.
    pub fn register<'a>(
        &self,
        registry: &mut ModuleRegistry<'a>,
        ruby: &'a mut RubySession,
        providers: RubyProviders<'a>,
    ) -> Result<(), ExpansionError> {
        let runtime = Rc::new(RefCell::new((ruby, providers)));
        for module in &self.entries {
            let cache = if module.inputs == Some(InputPolicy::Sdk) {
                ModuleCache::Ruby(Rc::clone(&runtime))
            } else {
                ModuleCache::Dynamic
            };
            let runtime = Rc::clone(&runtime);
            let repo_root = self.repo_root.clone();
            let entry = repo_root.join(&module.entry);
            registry.register_with_cache(
                module.definition.clone(),
                move |context| {
                    let (ruby, providers) = &mut *runtime.borrow_mut();
                    let evaluated = ruby
                        .evaluate_with_sdk(
                            EvalOperation::Expand,
                            &repo_root,
                            &entry,
                            ruby_context(context.options),
                            providers.reborrow(),
                        )
                        .map_err(|err| err.to_string())?;
                    if !evaluated.result.is_object() {
                        return Err("module output must be a JSON object".into());
                    }
                    let mut output: ModuleOutput =
                        serde_path_to_error::deserialize(evaluated.result)
                            .map_err(|err| format!("invalid module output: {err}"))?;
                    output.inputs.extend(evaluated.inputs);
                    Ok(output)
                },
                cache,
            )?;
        }
        Ok(())
    }
}

pub(super) fn ruby_context(config: &HostConfig) -> Value {
    let options: BTreeMap<_, _> = config
        .options
        .iter()
        .map(|(path, option)| (path, &option.value))
        .collect();
    let option_origins: BTreeMap<_, _> = config
        .options
        .iter()
        .filter_map(|(path, option)| {
            option
                .effective_definitions
                .first()
                .map(|definition| (path, &definition.origin))
        })
        .collect();
    json!({ "options": options, "optionOrigins": option_origins })
}
