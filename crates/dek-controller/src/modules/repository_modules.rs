use std::{cell::RefCell, collections::BTreeMap, fs, num::NonZeroU32, path::PathBuf, rc::Rc};

use dek_core::{
    InputKind, InputObservation, Observation, OptionPath, Origin, protocol::EvalOperation,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    eval::{RubySession, TargetProvider},
    repo::{InputPolicy, Repository},
};

use super::{
    ExpansionError, ModuleDefinition, ModuleOutput, ModuleRegistry, SchemaError, SchemaRegistry,
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
        target: &'a mut dyn TargetProvider,
    ) -> Result<(), ExpansionError> {
        let runtime = Rc::new(RefCell::new((ruby, target)));
        for module in &self.entries {
            let runtime = Rc::clone(&runtime);
            let entry = self.repo_root.join(&module.entry);
            let relative_entry = module.entry.to_string_lossy().into_owned();
            registry.register(module.definition.clone(), move |context| {
                let options: BTreeMap<_, _> = context
                    .options
                    .options
                    .iter()
                    .map(|(path, option)| (path, &option.value))
                    .collect();
                let (ruby, target) = &mut *runtime.borrow_mut();
                let bytes = fs::read(&entry)
                    .map_err(|err| format!("cannot read {}: {err}", entry.display()))?;
                let evaluated = ruby
                    .evaluate(
                        EvalOperation::Expand,
                        &entry,
                        json!({ "options": options }),
                        &mut **target,
                    )
                    .map_err(|err| err.to_string())?;
                let mut output: ModuleOutput =
                    serde_path_to_error::deserialize(evaluated.result)
                        .map_err(|err| format!("invalid module output: {err}"))?;
                output.inputs.extend(evaluated.inputs);
                output.inputs.push(InputObservation {
                    kind: InputKind::File,
                    id: relative_entry.clone(),
                    observation: Observation::Digest(format!("sha256:{:x}", Sha256::digest(bytes))),
                    origin: None,
                });
                Ok(output)
            })?;
        }
        Ok(())
    }
}
