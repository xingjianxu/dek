//! Assemble an evaluated host and request a plan from the selected target.

use std::error::Error;

use dek_controller::{
    cache::CacheDigest,
    eval::{LanguageCollector, RubyProviders, RubySession, TargetProvider, evaluate_host},
    modules::{
        ExpandedResources, ModuleRegistry, assemble_resources, register_app_modules,
        register_key_modules, register_system_modules,
    },
    repo::{HostSelector, Repository, Target},
};
use dek_core::{HostConfig, InputKind, Plan};
use dek_target::{backend::BackendRegistry, planner::plan};

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
    let (name, entry) = repo.select_host(selector)?;
    if let Target::Ssh(alias) = &entry.target {
        return Err(
            format!("主机 {name:?} 使用 SSH 别名 {alias:?}，本阶段 plan 只支持本机目标").into(),
        );
    }
    let mut ruby = RubySession::new();
    let evaluated = evaluate_host(
        repo,
        selector,
        &mut LanguageCollector::new(&mut ruby, provider),
    )?;

    let mut modules = ModuleRegistry::default();
    register_app_modules(&mut modules)?;
    register_system_modules(&mut modules)?;
    register_key_modules(&mut modules)?;
    evaluated.repository_modules.register(
        &mut modules,
        &mut ruby,
        RubyProviders {
            target: provider,
            http: None,
        },
    )?;
    let expansions = modules.expand(repo.root(), &evaluated.config)?;
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
