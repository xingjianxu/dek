//! Assemble local apply and present the target executor's result.

use std::{
    error::Error,
    fs::File,
    io::{Read, Write},
    path::Path,
};

use dek_controller::{
    eval::TargetProvider,
    repo::{HostSelector, Repository, Target},
};
use dek_core::{
    InputKind, InputObservation, Observation, Plan,
    protocol::{EVALUATOR_PROTOCOL_VERSION, TargetQuery, TargetQueryKind},
};
use dek_target::{
    backend::BackendRegistry,
    executor::{ExecutionOutcome, ExecutionRequest, Executor, LoadedPlan, load_plan_for_execution},
    state::{ActionResult, PlanBinding, StateError, StateGuard, StateStore},
};

use crate::{
    evaluation::normal_cache,
    local_target::LocalEvaluationTarget,
    plan_output::write_plan,
    planning::{plan_host_with_cache, repository_id},
};

pub fn apply_local(
    out: &mut impl Write,
    repo: &Repository,
    selector: HostSelector<'_>,
    registry: &BackendRegistry<'_>,
    state_dir: &Path,
    reviewed_id: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    apply_local_with_loader(
        out,
        repo,
        selector,
        registry,
        state_dir,
        reviewed_id,
        load_plan_for_execution,
    )
}

pub fn apply_local_with_loader(
    out: &mut impl Write,
    repo: &Repository,
    selector: HostSelector<'_>,
    registry: &BackendRegistry<'_>,
    state_dir: &Path,
    reviewed_id: Option<&str>,
    load: impl FnOnce(&StateGuard<'_>, &str, PlanBinding<'_>) -> Result<LoadedPlan, StateError>,
) -> Result<(), Box<dyn Error>> {
    let (name, host) = repo.select_host(selector)?;
    if let Target::Ssh(alias) = &host.target {
        return Err(
            format!("主机 {name:?} 使用 SSH 别名 {alias:?}，本阶段 apply 只支持本机目标").into(),
        );
    }
    let store = StateStore::open(state_dir)?;
    let repository_id = repository_id(repo);
    let binding = PlanBinding {
        repository_id: &repository_id,
        logical_host: name,
        target: registry.target(),
    };
    let create_plan = || create_plan(repo, selector, registry, &store);
    let current;
    let plan_id = match reviewed_id {
        Some(id) => id,
        None => {
            current = create_plan()?;
            &current.id
        }
    };
    let mut nonce = [0; 16];
    File::open("/dev/urandom")?.read_exact(&mut nonce)?;
    let request_id = format!("{:032x}", u128::from_ne_bytes(nonce));
    let mut provider = LocalEvaluationTarget(registry);
    let outcome = Executor::new(&store, registry).apply_with_loader(
        ExecutionRequest {
            binding,
            plan_id,
            request_id: &request_id,
        },
        |input| observe(&mut provider, input),
        |id| {
            writeln!(out, "run_id {id}")?;
            out.flush()
        },
        load,
    )?;

    match outcome {
        ExecutionOutcome::Finished(run) => {
            let changes = run
                .actions
                .iter()
                .filter(|action| action.result == ActionResult::Verified { changed: true })
                .count();
            writeln!(out, "已核验完成，实际变更 {changes} 项")?;
            Ok(())
        }
        ExecutionOutcome::ReviewRequired {
            reason,
            replacement,
        } => {
            let plan = match replacement {
                Some(plan) => *plan,
                None => create_plan()?,
            };
            writeln!(out, "{reason}。请审阅新计划后使用 dek apply --plan")?;
            write_plan(&mut *out, name, &plan, false)?;
            out.flush()?;
            Err(format!("等待审阅计划 {}", plan.id).into())
        }
    }
}

fn create_plan(
    repo: &Repository,
    selector: HostSelector<'_>,
    registry: &BackendRegistry<'_>,
    store: &StateStore,
) -> Result<Plan, Box<dyn Error>> {
    let cache = normal_cache(repo)?;
    let mut provider = LocalEvaluationTarget(registry);
    let planned = plan_host_with_cache(repo, selector, registry, &mut provider, cache.as_ref())?;
    store.lock()?.save_plan(
        &planned.name,
        &planned.plan,
        &planned.expanded.resources,
        &planned.expanded.contents,
    )?;
    Ok(planned.plan)
}

fn observe(
    provider: &mut LocalEvaluationTarget<'_, '_>,
    input: &InputObservation,
) -> Result<Observation, String> {
    let kind = match input.kind {
        InputKind::TargetFact => TargetQueryKind::Fact,
        InputKind::TargetCapability => TargetQueryKind::Capability,
        InputKind::TargetFile => TargetQueryKind::ReadText,
        _ => return Err("已审阅计划包含非目标端输入".into()),
    };
    provider
        .query(&TargetQuery {
            protocol_version: EVALUATOR_PROTOCOL_VERSION,
            parent_id: "apply".into(),
            id: "recheck".into(),
            kind,
            key: input.id.clone(),
            origin: input.origin.clone(),
        })
        .map(|answer| answer.observation)
}
