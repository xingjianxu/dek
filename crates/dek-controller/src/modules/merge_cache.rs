use dek_core::{HostConfig, UnitSpec};
use serde_json::json;
use thiserror::Error;

use crate::{
    cache::{
        CacheDependency, CacheDigest, CacheEntry, CacheError, CacheStore, Publication, UnitKey,
        UnitStage, semantic_digest,
    },
    eval::{
        CollectedUnit, DeclarationGraph, collection_semantic_digest, observations, target_scope,
    },
};

use super::{
    MergeError, SchemaRegistry,
    provenance::{Sources, file_origin, map_config_origins},
};

#[derive(Debug, Error)]
pub enum CachedMergeError {
    #[error(transparent)]
    Merge(#[from] MergeError),
    #[error(transparent)]
    Cache(#[from] CacheError),
    #[error("configuration inputs changed while merging host {0:?}; retry")]
    Stale(String),
}

pub fn merge_cache_key(graph: &DeclarationGraph) -> UnitKey {
    UnitKey {
        stage: UnitStage::Merge,
        unit: graph.host.name.clone(),
        params: Default::default(),
        target: Some(host_scope(graph)),
    }
}

pub(super) fn host_scope(graph: &DeclarationGraph) -> String {
    target_scope(
        &graph.host.name,
        &graph.target,
        graph.host.target_id.as_deref(),
    )
}

impl SchemaRegistry {
    /// Merge the selected graph, reusing the successful result when its ordered
    /// unit dependencies and schema still match.
    pub fn merge_cached(
        &self,
        store: &CacheStore,
        graph: &DeclarationGraph,
    ) -> Result<HostConfig, CachedMergeError> {
        if !graph_current(store, graph)? {
            return Err(CachedMergeError::Stale(graph.host.name.clone()));
        }
        let units = || graph_units(graph);
        let signature = self.merge_signature();
        let key = merge_cache_key(graph);
        let dependencies = units()
            .map(|unit| CacheDependency::Unit {
                key: unit.dependency_key(&graph.host.name, &graph.target),
                semantic_digest: collection_semantic_digest(&unit.spec),
            })
            .collect::<Vec<_>>();
        let mut lookup = store.lookup::<HostConfig>(&key, &signature)?;
        let mut sources = Sources::default();
        let mut normalized = units()
            .map(|unit| normalize(unit, &mut sources))
            .collect::<Vec<_>>();
        if let Some(entry) = lookup.entry.take()
            && entry.dependencies == dependencies
        {
            return Ok(restore(entry.output, &sources));
        }

        let host = normalized.pop().expect("the graph has a host entry");
        let output = match self.merge(&normalized, &host) {
            Ok(output) => output,
            Err(_) => {
                let original = graph
                    .profiles
                    .iter()
                    .map(|unit| unit.spec.clone())
                    .collect::<Vec<_>>();
                return Ok(self.merge(&original, &graph.host.spec)?);
            }
        };
        let entry = CacheEntry {
            dependencies,
            semantic_digest: config_digest(&output),
            output,
        };
        let publication = store.publish(&lookup, &entry, |_| {
            graph_current(store, graph).map_err(Into::into)
        })?;
        if publication != Publication::Published {
            return Err(CachedMergeError::Stale(graph.host.name.clone()));
        }
        Ok(restore(entry.output, &sources))
    }
}

fn graph_units(graph: &DeclarationGraph) -> impl Iterator<Item = &CollectedUnit> {
    graph.profiles.iter().chain(std::iter::once(&graph.host))
}

pub(super) fn graph_current(
    store: &CacheStore,
    graph: &DeclarationGraph,
) -> Result<bool, CacheError> {
    for unit in graph_units(graph) {
        if let Some(revision) = unit.cache_revision {
            let key = unit.dependency_key(&graph.host.name, &graph.target);
            if store.revision(&key)? != Some(revision) {
                return Ok(false);
            }
        }
    }
    let inputs = graph_units(graph).flat_map(|unit| &unit.spec.inputs);
    observations::local_unchanged(store.repository_root(), inputs).map_err(|source| {
        CacheError::Recheck {
            path: store.record_path(&merge_cache_key(graph)),
            source: Box::new(source),
        }
    })
}

fn normalize(unit: &CollectedUnit, sources: &mut Sources) -> UnitSpec {
    UnitSpec {
        options: unit
            .spec
            .options
            .iter()
            .cloned()
            .map(|mut assignment| {
                assignment.origin = sources.reference(&assignment.origin);
                assignment
            })
            .collect(),
        ..UnitSpec::default()
    }
}

fn restore(mut config: HostConfig, sources: &Sources) -> HostConfig {
    map_config_origins(&mut config, |origin| sources.resolve(origin));
    config
}

fn config_digest(config: &HostConfig) -> CacheDigest {
    let mut config = config.clone();
    map_config_origins(&mut config, file_origin);
    semantic_digest(&json!(config))
}
