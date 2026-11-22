//! 采集主机和 profile 声明。

mod cached;
mod graph;
mod host;
mod json;
mod languages;
pub(crate) mod observations;
mod ruby;
mod ruby_runtime;

pub(crate) use cached::target_scope;
pub use cached::{CachedCollector, collection_semantic_digest};
pub use graph::{
    CollectError, CollectRequest, CollectedUnit, Collector, DeclarationGraph, GraphError,
    collect_graph,
};
pub use host::{EvaluatedHost, HostEvaluationError, evaluate_host, evaluate_host_cached};
pub use json::JsonCollector;
pub use languages::LanguageCollector;
pub use ruby::{
    EvalOutput, HttpInputProvider, RubyError, RubyProviders, RubySession, TargetAnswer,
    TargetProvider, TargetSnapshotSource,
};
