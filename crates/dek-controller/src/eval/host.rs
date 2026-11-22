use dek_core::HostConfig;
use thiserror::Error;

use crate::{
    modules::{
        MergeError, RepositoryModuleError, RepositoryModules, SchemaError, SchemaRegistry,
        register_app_schemas, register_key_schemas, register_system_schemas,
    },
    repo::{HostSelector, Repository},
};

use super::{Collector, DeclarationGraph, GraphError, collect_graph};

#[derive(Debug)]
pub struct EvaluatedHost {
    pub graph: DeclarationGraph,
    pub config: HostConfig,
    pub repository_modules: RepositoryModules,
}

#[derive(Debug, Error)]
pub enum HostEvaluationError {
    #[error(transparent)]
    BuiltinSchema(#[from] SchemaError),
    #[error(transparent)]
    RepositorySchema(#[from] RepositoryModuleError),
    #[error(transparent)]
    Collect(#[from] GraphError),
    #[error(transparent)]
    Merge(#[from] MergeError),
}

/// 采集选定主机及其依赖并合并选项，返回可供后续展开的模块注册。
pub fn evaluate_host(
    repo: &Repository,
    selector: HostSelector<'_>,
    collector: &mut dyn Collector,
) -> Result<EvaluatedHost, HostEvaluationError> {
    let mut schemas = SchemaRegistry::default();
    register_app_schemas(&mut schemas)?;
    register_system_schemas(&mut schemas)?;
    register_key_schemas(&mut schemas)?;
    let repository_modules = RepositoryModules::load(repo, &mut schemas)?;

    let graph = collect_graph(repo, selector, collector)?;
    let config = schemas.merge_profiles(
        graph.profiles.iter().map(|profile| &profile.spec),
        &graph.host.spec,
    )?;
    Ok(EvaluatedHost {
        graph,
        config,
        repository_modules,
    })
}
