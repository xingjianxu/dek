//! Invoke the same execution and recovery paths as local apply.

use std::{error::Error, io, path::Path, sync::Mutex};

use dek_core::target_protocol::{
    ExecutionHost, RunQuery, TargetEvent, TargetRequest, TargetResult,
};

use crate::{
    backend::BackendRegistry,
    executor::{ExecutionOutcome, ExecutionRequest, Executor},
    inputs,
    state::{PlanBinding, StateStore},
    terminal::{TerminalCommand, run_in_terminal},
};

use super::EventSender;

pub fn dispatch_execution(
    registry: &BackendRegistry<'_>,
    root: &Path,
    operation: TargetRequest,
    event: &mut EventSender<'_>,
) -> Result<TargetResult, TargetRequest> {
    let result = match operation {
        TargetRequest::RunStatus { host, query } => status(registry, root, &host, query),
        TargetRequest::ApplyPlan {
            host,
            plan_id,
            request_id,
            interactive,
        } => execute(
            registry,
            root,
            &host,
            interactive,
            event,
            |executor, binding, started| {
                executor.apply(
                    ExecutionRequest {
                        binding,
                        plan_id: &plan_id,
                        request_id: &request_id,
                    },
                    |input| inputs::reobserve(registry, input),
                    started,
                )
            },
        ),
        TargetRequest::ResumeRun {
            host,
            run_id,
            interactive,
        } => execute(
            registry,
            root,
            &host,
            interactive,
            event,
            |executor, binding, started| {
                executor.resume(
                    binding,
                    &run_id,
                    |input| inputs::reobserve(registry, input),
                    started,
                )
            },
        ),
        other => return Err(other),
    };
    Ok(result.unwrap_or_else(|error| TargetResult::Error {
        message: error.to_string(),
    }))
}

fn status(
    registry: &BackendRegistry<'_>,
    root: &Path,
    host: &ExecutionHost,
    query: RunQuery,
) -> Result<TargetResult, Box<dyn Error>> {
    let store = open_store(root)?;
    let binding = binding(registry, host)?;
    let guard = store.lock()?;
    let run = match query {
        RunQuery::Latest => guard.latest_run(binding)?,
        RunQuery::Run(id) => Some(guard.load_run(&id, binding)?),
        RunQuery::Request(id) => guard.run_for_request(binding, &id)?,
    };
    Ok(TargetResult::ExecutionStatus(
        run.map(|run| Box::new(run.into_status())),
    ))
}

fn execute(
    registry: &BackendRegistry<'_>,
    root: &Path,
    host: &ExecutionHost,
    interactive: bool,
    event: &mut EventSender<'_>,
    execution: impl FnOnce(
        &Executor<'_, '_>,
        PlanBinding<'_>,
        &mut dyn FnMut(&str) -> io::Result<()>,
    ) -> Result<ExecutionOutcome, crate::executor::ExecutionError>,
) -> Result<TargetResult, Box<dyn Error>> {
    let store = open_store(root)?;
    let binding = binding(registry, host)?;
    let event = Mutex::new(event);
    let send = |message| {
        let mut event = event
            .lock()
            .map_err(|_| io::Error::other("执行事件锁已损坏"))?;
        event(message)
    };
    let terminal = |command: &TerminalCommand| {
        run_in_terminal(root, command, |token| {
            send(TargetEvent::TerminalRequired { token })
        })
    };
    let executor = Executor::new(&store, registry);
    let executor = if interactive {
        executor.with_terminal(&terminal)
    } else {
        executor
    };
    let result = execution(&executor, binding, &mut |run_id| {
        send(TargetEvent::RunStarted {
            run_id: run_id.to_owned(),
        })
    })?;
    Ok(match result {
        ExecutionOutcome::Finished(run) => {
            TargetResult::ExecutionFinished(Box::new(run.into_status()))
        }
        ExecutionOutcome::ReviewRequired {
            reason,
            replacement,
        } => TargetResult::ReviewRequired {
            reason: reason.to_string(),
            replacement,
            mode: reason.replan_mode(),
        },
    })
}

fn binding<'a>(
    registry: &'a BackendRegistry<'_>,
    host: &'a ExecutionHost,
) -> Result<PlanBinding<'a>, Box<dyn Error>> {
    if host.repository_id.trim().is_empty() || host.logical_host.trim().is_empty() {
        return Err("远程执行需要仓库标识和清单主机名".into());
    }
    Ok(PlanBinding {
        target: registry.target(),
        repository_id: &host.repository_id,
        logical_host: &host.logical_host,
    })
}

fn open_store(root: &Path) -> Result<StateStore, Box<dyn Error>> {
    StateStore::open(root)
        .map_err(|error| format!("{error}。请在目标机配置 SSH 账户持有的私有状态目录").into())
}
