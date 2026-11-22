//! Assemble an evaluated host and request a plan from the selected target.

use std::error::Error;

use dek_controller::{
    cache::{CacheDigest, CacheStore, CachedHttpInputs},
    eval::{HttpInputProvider, RubyProviders, RubySession, TargetProvider},
    modules::{ExpandedResources, assemble_resources},
    repo::{HostSelector, Repository, Target},
};
use dek_core::{HostConfig, InputKind, Plan};
use dek_target::{backend::BackendRegistry, planner::plan};

use crate::evaluation::{evaluate_options, expand_host};

pub struct PlannedHost {
    pub name: String,
    pub config: HostConfig,
    pub expanded: ExpandedResources,
    pub plan: Plan,
}

pub fn plan_host(
    repo: &Repository,
    selector: HostSelector<'_>,
    registry: &BackendRegistry<'_>,
    provider: &mut dyn TargetProvider,
) -> Result<PlannedHost, Box<dyn Error>> {
    plan_host_with_cache(repo, selector, registry, provider, None)
}

pub fn plan_host_with_cache(
    repo: &Repository,
    selector: HostSelector<'_>,
    registry: &BackendRegistry<'_>,
    provider: &mut dyn TargetProvider,
    cache: Option<&CacheStore>,
) -> Result<PlannedHost, Box<dyn Error>> {
    let (name, entry) = repo.select_host(selector)?;
    if let Target::Ssh(alias) = &entry.target {
        return Err(
            format!("主机 {name:?} 使用 SSH 别名 {alias:?}，本阶段 plan 只支持本机目标").into(),
        );
    }
    let mut ruby = RubySession::new();
    let mut http = cache.map(CachedHttpInputs::new);
    let mut providers = RubyProviders {
        target: provider,
        http: http
            .as_mut()
            .map(|inputs| inputs as &mut dyn HttpInputProvider),
    };
    let evaluated = evaluate_options(repo, selector, &mut ruby, providers.reborrow(), cache)?;
    let expansions = expand_host(repo.root(), &evaluated, &mut ruby, providers, cache)?;
    let expanded = assemble_resources(repo.root(), &expansions)?;

    let mut used_inputs = Vec::new();
    let observed = std::iter::once(&evaluated.graph.host)
        .chain(&evaluated.graph.profiles)
        .flat_map(|unit| &unit.spec.inputs)
        .chain(expansions.iter().flat_map(|module| &module.output.inputs));
    for input in observed {
        if matches!(
            input.kind,
            InputKind::TargetFact | InputKind::TargetCapability | InputKind::TargetFile
        ) && !used_inputs.contains(input)
        {
            used_inputs.push(input.clone());
        }
    }
    let repository_id =
        CacheDigest::of_bytes(repo.root().as_os_str().as_encoded_bytes()).to_string();
    let plan = plan(registry, &repository_id, &expanded.resources, &used_inputs)?;
    Ok(PlannedHost {
        name: name.into(),
        config: evaluated.config,
        expanded,
        plan,
    })
}
