//! Present persisted execution records without evaluating the configuration.

use std::{error::Error, io::Write, path::Path};

use dek_controller::repo::{HostSelector, Repository, Target};
use dek_core::{ActionKind, RunStatus, TargetIdentity};
use dek_target::state::{ActionResult, PlanBinding, StateStore};
use serde_json::json;

use crate::{plan_output::description, planning::repository_id};

pub(crate) fn show_status(
    out: impl Write,
    repo: &Repository,
    selector: HostSelector<'_>,
    target: &TargetIdentity,
    state_dir: &Path,
    run_id: Option<&str>,
    json_output: bool,
) -> Result<(), Box<dyn Error>> {
    let (name, entry) = repo.select_host(selector)?;
    if let Target::Ssh(alias) = &entry.target {
        return Err(
            format!("主机 {name:?} 使用 SSH 别名 {alias:?}，本阶段 status 只支持本机目标").into(),
        );
    }
    let store = StateStore::open(state_dir)?;
    let repository_id = repository_id(repo);
    let binding = PlanBinding {
        repository_id: &repository_id,
        logical_host: name,
        target,
    };
    let guard = store.lock()?;
    let record = match run_id {
        Some(id) => Some(guard.load_run(id, binding)?),
        None => guard.latest_run(binding)?,
    };
    drop(guard);
    write_status(out, record.map(|run| run.into_status()), None, json_output)
}

pub(crate) fn write_status(
    mut out: impl Write,
    record: Option<RunStatus>,
    alias: Option<&str>,
    json_output: bool,
) -> Result<(), Box<dyn Error>> {
    let Some(run) = record else {
        writeln!(
            out,
            "{}",
            if json_output {
                "null"
            } else {
                "暂无执行记录"
            }
        )?;
        return Ok(());
    };
    if json_output {
        let document = json!({
            "run_id": run.id,
            "plan_id": run.plan_id,
            "host": run.logical_host,
            "target": run.target,
            "request_id": run.request_id,
            "created_at": run.created_at,
            "finished": run.finished,
            "actions": run.actions,
            "pending_notifications": run.pending_notifications,
        });
        serde_json::to_writer_pretty(&mut out, &document)?;
        writeln!(out)?;
        return Ok(());
    }

    if let Some(alias) = alias {
        writeln!(
            out,
            "主机 {}（SSH：{alias}，目标 {}）",
            run.logical_host, run.target.id
        )?;
    } else {
        writeln!(out, "主机 {}（本机 {}）", run.logical_host, run.target.id)?;
    }
    writeln!(out, "执行 {}", run.id)?;
    writeln!(out, "计划 {}", run.plan_id)?;
    for item in &run.actions {
        let label = match item.result {
            ActionResult::Verified { .. } => "已核验",
            ActionResult::Failed { .. } => "失败",
            ActionResult::Unverified => "结果待核验",
            ActionResult::Deferred { .. } => "待激活",
            ActionResult::Pending => "待执行",
        };
        let operation = match (&item.action.kind, item.selected_notification_effect) {
            (ActionKind::NotifyService { unit, .. }, Some(effect)) => {
                description(&ActionKind::NotifyService {
                    unit: unit.clone(),
                    effect,
                    sources: Vec::new(),
                })
            }
            (_, _) => description(&item.action.kind),
        };
        writeln!(out, "{label} [{}] {operation}", item.action.id)?;
        if let ActionResult::Failed { message, .. } | ActionResult::Deferred { message } =
            &item.result
        {
            writeln!(out, "  {message}")?;
        }
        if let Some(diagnostic) = &item.recovery_diagnostic {
            writeln!(out, "  {diagnostic}")?;
        }
        if !item.action.depends_on.is_empty() {
            writeln!(out, "  依赖 {}", item.action.depends_on.join(", "))?;
        }
    }
    for notice in &run.pending_notifications {
        writeln!(
            out,
            "待续办服务通知 [{}] {}",
            notice.action_id, notice.unit.name
        )?;
    }
    Ok(())
}
