use std::{error::Error, io::Write};

use dek_controller::{
    repo::{HostSelector, Repository},
    transport::{SshErrorKind, SshSession, attach_terminal},
};
use dek_core::{
    PlanMode, RunStatus,
    target_protocol::{ExecutionHost, RunQuery, TargetEvent, TargetRequest, TargetResult},
};

use crate::{
    applying::{
        ApplySelection, needs_upgrade_review, new_request_id, write_finished, write_upgrade_review,
    },
    evaluation::normal_cache,
    plan_output::write_plan,
    planning::{plan_remote_host, repository_id},
    status::write_status,
};

pub(crate) fn apply_remote(
    out: &mut impl Write,
    repo: &Repository,
    name: &str,
    alias: &str,
    target_id: &str,
    selection: ApplySelection<'_>,
    interactive: bool,
) -> Result<(), Box<dyn Error>> {
    if selection.request_id.is_some_and(|id| id.trim().is_empty()) {
        return Err("请求标识不能为空".into());
    }
    let host = execution_host(repo, name);
    let mut session = SshSession::connect(alias, target_id)?;
    let resume = if let Some(id) = selection.run {
        Some(id.to_owned())
    } else if selection.plan.is_none() {
        status(&mut session, &host, RunQuery::Latest)?
            .filter(|run| !run.finished)
            .map(|run| run.id)
    } else {
        None
    };
    let (operation, recovery) = if let Some(run_id) = resume {
        let recovery = Recovery::Run(run_id.clone());
        (
            TargetRequest::ResumeRun {
                host: host.clone(),
                run_id,
                interactive,
            },
            recovery,
        )
    } else {
        let plan_id = match selection.plan {
            Some(id) => id.to_owned(),
            None => {
                let review = plan_remote_host(
                    repo,
                    HostSelector::Named(name),
                    &mut session,
                    normal_cache(repo)?.as_ref(),
                    PlanMode::Normal,
                )?;
                if needs_upgrade_review(&review.plan) {
                    session.finish()?;
                    return write_upgrade_review(out, name, &review.plan);
                }
                review.plan.id
            }
        };
        let request_id = match selection.request_id {
            Some(id) => id.to_owned(),
            None => new_request_id()?,
        };
        // A caller that loses the link before RunStarted can still replay the request.
        if selection.request_id.is_none() {
            writeln!(out, "plan_id {plan_id}")?;
            writeln!(out, "request_id {request_id}")?;
            out.flush()?;
        }
        let recovery = Recovery::Request {
            plan_id: plan_id.clone(),
            request_id: request_id.clone(),
        };
        (
            TargetRequest::ApplyPlan {
                host: host.clone(),
                plan_id,
                request_id,
                interactive,
            },
            recovery,
        )
    };

    let mut started = None;
    let result = session.request_with_events(operation, |event| match event {
        TargetEvent::RunStarted { run_id } if started.is_none() => {
            writeln!(out, "run_id {run_id}").map_err(SshErrorKind::Io)?;
            started = Some(run_id);
            out.flush().map_err(SshErrorKind::Io)
        }
        TargetEvent::TerminalRequired { token } if interactive && started.is_some() => {
            attach_terminal(alias, &token)
        }
        _ => Err(SshErrorKind::Target("目标返回了意外的执行事件".into())),
    });
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            let lookup = started
                .clone()
                .map(RunQuery::Run)
                .unwrap_or_else(|| recovery.query());
            let recovered = SshSession::connect(alias, target_id)
                .ok()
                .and_then(|mut session| status(&mut session, &host, lookup).ok().flatten())
                .map(|record| record.id);
            return Err(format!(
                "{error}。{}",
                recovery.hint(name, recovered.as_deref().or(started.as_deref()))
            )
            .into());
        }
    };
    match result {
        TargetResult::ExecutionFinished(run) if started.as_deref() == Some(&run.id) => {
            session.finish()?;
            write_finished(out, &run.actions)
        }
        TargetResult::ReviewRequired {
            reason,
            replacement,
            mode,
        } => {
            let plan = match replacement {
                Some(plan) => *plan,
                None => {
                    plan_remote_host(
                        repo,
                        HostSelector::Named(name),
                        &mut session,
                        normal_cache(repo)?.as_ref(),
                        mode,
                    )?
                    .plan
                }
            };
            session.finish()?;
            writeln!(
                out,
                "{reason}。请审阅新计划并运行 dek apply -H {name} --plan {}",
                plan.id
            )?;
            write_plan(&mut *out, name, &plan, false)?;
            out.flush()?;
            Err(format!("等待审阅计划 {}", plan.id).into())
        }
        TargetResult::Error { message } => {
            if let Some(run_id) = started {
                Err(format!("{message}。{}", recovery.hint(name, Some(&run_id))).into())
            } else {
                Err(message.into())
            }
        }
        _ => Err("目标返回了意外的执行结果".into()),
    }
}

pub(crate) fn status_remote(
    out: impl Write,
    repo: &Repository,
    name: &str,
    alias: &str,
    target_id: &str,
    run_id: Option<&str>,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    let mut session = SshSession::connect(alias, target_id)?;
    let query = run_id.map_or(RunQuery::Latest, |id| RunQuery::Run(id.into()));
    let record = status(&mut session, &execution_host(repo, name), query)?;
    session.finish()?;
    write_status(out, record, Some(alias), json)
}

fn status(
    session: &mut SshSession,
    host: &ExecutionHost,
    query: RunQuery,
) -> Result<Option<RunStatus>, Box<dyn Error>> {
    let result = session.request(TargetRequest::RunStatus {
        host: host.clone(),
        query,
    })?;
    match result {
        TargetResult::ExecutionStatus(record) => Ok(record.map(|status| *status)),
        TargetResult::Error { message } => Err(message.into()),
        _ => Err("目标返回了意外的执行状态".into()),
    }
}

fn execution_host(repo: &Repository, name: &str) -> ExecutionHost {
    ExecutionHost {
        repository_id: repository_id(repo),
        logical_host: name.into(),
    }
}

enum Recovery {
    Run(String),
    Request { plan_id: String, request_id: String },
}

impl Recovery {
    fn query(&self) -> RunQuery {
        match self {
            Self::Run(id) => RunQuery::Run(id.clone()),
            Self::Request { request_id, .. } => RunQuery::Request(request_id.clone()),
        }
    }

    fn hint(&self, name: &str, run: Option<&str>) -> String {
        if let Some(id) = run {
            return format!(
                "请用 dek status -H {name:?} --run {id:?} 查询，或用 dek apply -H {name:?} --run {id:?} 续办"
            );
        }
        match self {
            Self::Run(id) => format!("请用 dek status -H {name:?} --run {id:?} 查询"),
            Self::Request {
                plan_id,
                request_id,
            } => format!(
                "请用 dek apply -H {name:?} --plan {plan_id:?} --request-id {request_id:?} 重发原请求"
            ),
        }
    }
}
