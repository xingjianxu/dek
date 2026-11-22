//! Assemble local apply and present the target executor's result.

use std::{
    error::Error,
    fs::File,
    io::{Read, Write},
    path::Path,
};

use dek_controller::repo::{HostSelector, Repository, Target};
use dek_core::{ActionResult, Plan, PlanMode, RunAction};
use dek_target::{
    backend::BackendRegistry,
    executor::{ExecutionOutcome, ExecutionRequest, Executor, LoadedPlan, load_plan_for_execution},
    inputs::reobserve,
    state::{PlanBinding, StateError, StateGuard, StateStore},
};

use crate::{
    evaluation::normal_cache,
    local_target::LocalEvaluationTarget,
    plan_output::write_plan,
    planning::{plan_host_with_cache_mode, repository_id},
};

#[derive(Clone, Copy, Default)]
pub struct ApplySelection<'a> {
    pub plan: Option<&'a str>,
    pub run: Option<&'a str>,
    pub request_id: Option<&'a str>,
}

pub fn apply_local(
    out: &mut impl Write,
    repo: &Repository,
    selector: HostSelector<'_>,
    registry: &BackendRegistry<'_>,
    state_dir: &Path,
    selection: ApplySelection<'_>,
) -> Result<(), Box<dyn Error>> {
    apply_local_with_loader(
        out,
        repo,
        selector,
        registry,
        state_dir,
        selection,
        load_plan_for_execution,
    )
}

pub fn apply_local_with_loader(
    out: &mut impl Write,
    repo: &Repository,
    selector: HostSelector<'_>,
    registry: &BackendRegistry<'_>,
    state_dir: &Path,
    selection: ApplySelection<'_>,
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
    let create_plan = || create_plan(repo, selector, registry, &store, PlanMode::Normal);
    let resume = if let Some(id) = selection.run {
        Some(id.to_owned())
    } else if selection.plan.is_none() {
        store
            .lock()?
            .latest_run(binding)?
            .filter(|run| !run.finished)
            .map(|run| run.id)
    } else {
        None
    };

    let executor = Executor::new(&store, registry);
    let started = |id: &str| {
        if resume.is_some() {
            writeln!(out, "恢复已审阅的执行 {id}")?;
        }
        writeln!(out, "run_id {id}")?;
        out.flush()
    };
    let outcome = if let Some(id) = &resume {
        executor.resume(binding, id, |input| reobserve(registry, input), started)?
    } else {
        let current;
        let plan_id = match selection.plan {
            Some(id) => id,
            None => {
                current = create_plan()?;
                if needs_upgrade_review(&current) {
                    return write_upgrade_review(out, name, &current);
                }
                &current.id
            }
        };
        let generated;
        let request_id = match selection.request_id {
            Some(id) if id.trim().is_empty() => return Err("请求标识不能为空".into()),
            Some(id) => id,
            None => {
                generated = new_request_id()?;
                &generated
            }
        };
        executor.apply_with_loader(
            ExecutionRequest {
                binding,
                plan_id,
                request_id,
            },
            |input| reobserve(registry, input),
            started,
            load,
        )?
    };

    match outcome {
        ExecutionOutcome::Finished(run) => write_finished(out, &run.actions),
        ExecutionOutcome::ReviewRequired {
            reason,
            replacement,
        } => {
            let plan = match replacement {
                Some(plan) => *plan,
                None if resume.is_some() => {
                    return Err(format!("{reason}。请运行 dek plan 重新审阅").into());
                }
                None => crate::applying::create_plan(
                    repo,
                    selector,
                    registry,
                    &store,
                    reason.replan_mode(),
                )?,
            };
            writeln!(
                out,
                "{reason}。请审阅新计划并运行 dek apply -H {name} --plan {}",
                plan.id
            )?;
            write_plan(&mut *out, name, &plan, false)?;
            out.flush()?;
            Err(format!("等待审阅计划 {}", plan.id).into())
        }
    }
}

pub(crate) fn create_plan(
    repo: &Repository,
    selector: HostSelector<'_>,
    registry: &BackendRegistry<'_>,
    store: &StateStore,
    mode: PlanMode,
) -> Result<Plan, Box<dyn Error>> {
    let cache = normal_cache(repo)?;
    let mut provider = LocalEvaluationTarget(registry);
    let planned = plan_host_with_cache_mode(
        repo,
        selector,
        registry,
        &mut provider,
        cache.as_ref(),
        mode,
    )?;
    Ok(store.lock()?.save_plan(
        &planned.name,
        &planned.plan,
        &planned.expanded.resources,
        &planned.expanded.contents,
    )?)
}

pub(crate) fn new_request_id() -> std::io::Result<String> {
    let mut nonce = [0; 16];
    File::open("/dev/urandom")?.read_exact(&mut nonce)?;
    Ok(format!("{:032x}", u128::from_ne_bytes(nonce)))
}

pub(crate) fn write_finished(
    mut out: impl Write,
    actions: &[RunAction],
) -> Result<(), Box<dyn Error>> {
    let changes = actions
        .iter()
        .filter(|action| action.result == ActionResult::Verified { changed: true })
        .count();
    let deferred = actions
        .iter()
        .filter(|action| matches!(action.result, ActionResult::Deferred { .. }))
        .count();
    if deferred == 0 {
        writeln!(out, "已核验完成，实际变更 {changes} 项")?;
    } else {
        writeln!(out, "实际变更 {changes} 项，待激活 {deferred} 项")?;
    }
    Ok(())
}

pub(crate) fn needs_upgrade_review(plan: &Plan) -> bool {
    plan.mode() == PlanMode::Upgrade
}

pub(crate) fn write_upgrade_review(
    mut out: impl Write,
    name: &str,
    plan: &Plan,
) -> Result<(), Box<dyn Error>> {
    write_plan(&mut out, name, plan, false)?;
    writeln!(
        out,
        "请审阅升级计划并运行 dek apply -H {name} --plan {}",
        plan.id
    )?;
    out.flush()?;
    Err(format!("等待审阅计划 {}", plan.id).into())
}
