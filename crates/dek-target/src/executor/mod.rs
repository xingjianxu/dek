//! Recheck approved plans and execute them using the target-owned journal.

mod scope;

use std::{collections::BTreeSet, io};

use dek_core::{Action, ActionKind, InputObservation, Observation, Plan, ResourceKind};
use thiserror::Error;

use crate::{
    backend::{ActionContext, ActionReadiness, Backend, BackendRegistry},
    planner::{self, PlanError},
    state::{
        ActionAttempt, ActionReceipt, ActionResult, PlanBinding, ReviewedPlan, RunRecord,
        StateError, StateGuard, StateStore,
    },
};

use scope::{Scope, match_scope, validate_order};

pub struct ExecutionRequest<'a> {
    pub binding: PlanBinding<'a>,
    pub plan_id: &'a str,
    pub request_id: &'a str,
}

/// A reader can distinguish policy expiry from invalid binding or corrupt records.
pub enum LoadedPlan {
    Approved(Box<ReviewedPlan>),
    Expired,
}

#[derive(Debug)]
pub enum ReviewReason {
    Expired,
    InputChanged(String),
    ScopeExpanded,
}

impl std::fmt::Display for ReviewReason {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Expired => out.write_str("已审阅计划过期"),
            Self::InputChanged(input) => write!(out, "已审阅的目标输入 {input} 发生变化"),
            Self::ScopeExpanded => out.write_str("目标实际所需的变更超出已审阅范围"),
        }
    }
}

#[derive(Debug)]
pub enum ExecutionOutcome {
    Finished(Box<RunRecord>),
    ReviewRequired {
        reason: ReviewReason,
        replacement: Option<Box<Plan>>,
    },
}

impl ExecutionOutcome {
    fn review(reason: ReviewReason) -> Self {
        Self::ReviewRequired {
            reason,
            replacement: None,
        }
    }
}

#[derive(Debug, Error)]
pub enum ExecutionError {
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error("动作 {id}（{resource}）：{message}")]
    Action {
        id: String,
        resource: String,
        message: String,
    },
    #[error("无法复核目标输入 {input:?}：{message}。请重新规划")]
    Input { input: String, message: String },
    #[error("执行记录 {0} 的动作与已审阅计划不符，请重新规划")]
    RunMismatch(String),
    #[error("无法输出执行 ID：{0}")]
    Output(#[source] io::Error),
}

impl ExecutionError {
    fn action(action: &Action, message: impl Into<String>) -> Self {
        Self::Action {
            id: action.id.clone(),
            resource: format!("{:?}", action.kind),
            message: message.into(),
        }
    }
}

pub struct Executor<'a, 'backend> {
    store: &'a StateStore,
    registry: &'a BackendRegistry<'backend>,
}

impl<'a, 'backend> Executor<'a, 'backend> {
    pub fn new(store: &'a StateStore, registry: &'a BackendRegistry<'backend>) -> Self {
        Self { store, registry }
    }

    pub fn apply(
        &self,
        request: ExecutionRequest<'_>,
        observe_input: impl FnMut(&InputObservation) -> Result<Observation, String>,
        started: impl FnOnce(&str) -> io::Result<()>,
    ) -> Result<ExecutionOutcome, ExecutionError> {
        self.apply_with_loader(request, observe_input, started, load_plan_for_execution)
    }

    pub fn apply_with_loader(
        &self,
        request: ExecutionRequest<'_>,
        mut observe_input: impl FnMut(&InputObservation) -> Result<Observation, String>,
        started: impl FnOnce(&str) -> io::Result<()>,
        load: impl FnOnce(&StateGuard<'_>, &str, PlanBinding<'_>) -> Result<LoadedPlan, StateError>,
    ) -> Result<ExecutionOutcome, ExecutionError> {
        let _lease = self.store.execution_lease()?;
        let loaded = {
            let guard = self.store.lock()?;
            load(&guard, request.plan_id, request.binding)?
        };
        let LoadedPlan::Approved(reviewed) = loaded else {
            return Ok(ExecutionOutcome::review(ReviewReason::Expired));
        };
        planner::check_compatibility(self.registry, &reviewed.plan)?;
        validate_order(&reviewed.plan)?;

        for input in &reviewed.plan.used_inputs {
            let actual = observe_input(input).map_err(|message| ExecutionError::Input {
                input: input.id.clone(),
                message,
            })?;
            if actual != input.observation {
                return Ok(ExecutionOutcome::review(ReviewReason::InputChanged(
                    input.id.clone(),
                )));
            }
        }

        let fresh = planner::plan_for_host(
            self.registry,
            request.binding.repository_id,
            request.binding.logical_host,
            &reviewed.resources,
            &reviewed.plan.used_inputs,
        )?;
        let Scope::Reviewed { live, conflict } = match_scope(&reviewed.plan, &fresh) else {
            let fresh = self.store.lock()?.save_plan(
                request.binding.logical_host,
                &fresh,
                &reviewed.resources,
                &reviewed.contents,
            )?;
            return Ok(ExecutionOutcome::ReviewRequired {
                reason: ReviewReason::ScopeExpanded,
                replacement: Some(Box::new(fresh)),
            });
        };
        for action in &reviewed.plan.actions {
            self.backend(action)?
                .validate_action(action)
                .map_err(|error| ExecutionError::action(action, error.0))?;
        }

        let run =
            self.store
                .lock()?
                .begin_run(request.plan_id, request.binding, request.request_id);
        let run = match run {
            Err(StateError::ExpiredPlan { .. }) => {
                return Ok(ExecutionOutcome::review(ReviewReason::Expired));
            }
            result => result?,
        };
        if !run
            .actions
            .iter()
            .map(|item| &item.action)
            .eq(&reviewed.plan.actions)
        {
            return Err(ExecutionError::RunMismatch(run.id));
        }
        started(&run.id).map_err(ExecutionError::Output)?;
        if run.finished {
            return Ok(ExecutionOutcome::Finished(Box::new(run)));
        }
        if let Some(action) = conflict {
            let failure = ExecutionError::action(action, "前置条件已改变，请重新规划");
            let attempt = self
                .store
                .lock()?
                .begin_action(&run.id, request.binding, &action.id)?;
            if let ActionAttempt::Started(attempt) = attempt {
                self.record(&run, request.binding, action, attempt, &Err(&failure))?;
            }
            return Err(failure);
        }

        self.run_actions(&run, request.binding, &reviewed, &live)?;
        let guard = self.store.lock()?;
        guard.finish_run(&run.id, request.binding)?;
        Ok(ExecutionOutcome::Finished(Box::new(
            guard.load_run(&run.id, request.binding)?,
        )))
    }

    fn run_actions(
        &self,
        run: &RunRecord,
        binding: PlanBinding<'_>,
        reviewed: &ReviewedPlan,
        live: &BTreeSet<&str>,
    ) -> Result<(), ExecutionError> {
        let mut changed = BTreeSet::new();
        for item in &run.actions {
            let action = &item.action;
            match item.result {
                ActionResult::Verified { changed: wrote } => {
                    if wrote {
                        changed.insert(action.id.as_str());
                    }
                    continue;
                }
                ActionResult::Failed { .. } => {
                    return Err(ExecutionError::action(
                        action,
                        "已有失败回执，请查询执行记录",
                    ));
                }
                ActionResult::Pending | ActionResult::Unverified => {}
            }

            let backend = self.backend(action)?;
            let no_trigger = matches!(action.kind, ActionKind::NotifyService { .. })
                && !live.contains(action.id.as_str())
                && !run
                    .pending_notifications
                    .iter()
                    .any(|notice| notice.action_id == action.id)
                && action
                    .depends_on
                    .iter()
                    .all(|id| !changed.contains(id.as_str()));
            let readiness = if no_trigger && item.result == ActionResult::Pending {
                Ok(ActionReadiness::Satisfied)
            } else {
                backend
                    .inspect_action(action)
                    .map_err(|error| ExecutionError::action(action, error.0))
            };
            let attempt = self
                .store
                .lock()?
                .begin_action(&run.id, binding, &action.id)?;
            match attempt {
                ActionAttempt::NeedsVerification(attempt) => {
                    if matches!(readiness, Ok(ActionReadiness::Satisfied))
                        && !is_effect(&action.kind)
                    {
                        self.record(run, binding, action, attempt, &Ok(false))?;
                    } else {
                        return Err(ExecutionError::action(action, "先前尝试的结果仍待核验"));
                    }
                }
                ActionAttempt::Started(attempt) => {
                    let outcome = self.dispatch(run, binding, reviewed, action, backend, readiness);
                    self.record(run, binding, action, attempt, &outcome.as_ref().copied())?;
                    if outcome? {
                        changed.insert(action.id.as_str());
                    }
                }
            }
        }
        Ok(())
    }

    fn dispatch(
        &self,
        run: &RunRecord,
        binding: PlanBinding<'_>,
        reviewed: &ReviewedPlan,
        action: &Action,
        backend: &dyn Backend,
        readiness: Result<ActionReadiness, ExecutionError>,
    ) -> Result<bool, ExecutionError> {
        match readiness? {
            ActionReadiness::Satisfied => return Ok(false),
            ActionReadiness::Conflict(reason) => {
                return Err(ExecutionError::action(action, reason));
            }
            ActionReadiness::Ready => {}
        }

        let notices = run
            .actions
            .iter()
            .filter_map(|item| {
                let notice = &item.action;
                (matches!(notice.kind, ActionKind::NotifyService { .. })
                    && notice.depends_on.contains(&action.id))
                .then_some(notice.id.as_str())
            })
            .collect::<Vec<_>>();
        if !notices.is_empty() {
            let guard = self.store.lock()?;
            for id in notices {
                guard.queue_notification(&run.id, binding, id)?;
            }
        }

        let context = ActionContext::new(action, &reviewed.contents);
        backend
            .execute(&context)
            .map_err(|error| ExecutionError::action(action, error.0))?;
        if !backend
            .verify(&action.kind)
            .map_err(|error| ExecutionError::action(action, error.0))?
        {
            return Err(ExecutionError::action(action, "执行后的状态核验未通过"));
        }
        Ok(true)
    }

    fn record(
        &self,
        run: &RunRecord,
        binding: PlanBinding<'_>,
        action: &Action,
        attempt: u32,
        result: &Result<bool, &ExecutionError>,
    ) -> Result<(), StateError> {
        let receipt = match result {
            Ok(changed) => ActionReceipt::Verified { changed: *changed },
            Err(error) => ActionReceipt::Failed {
                message: error.to_string(),
            },
        };
        self.store
            .lock()?
            .record_action_result(&run.id, binding, &action.id, attempt, receipt)
    }

    fn backend(&self, action: &Action) -> Result<&dyn Backend, ExecutionError> {
        let kind = action_kind(&action.kind);
        self.registry
            .backend(kind)
            .map(|(_, backend)| backend)
            .ok_or_else(|| ExecutionError::action(action, format!("缺少 {kind:?} 后端")))
    }
}

fn is_effect(action: &ActionKind) -> bool {
    matches!(
        action,
        ActionKind::ReloadManager(_) | ActionKind::NotifyService { .. }
    )
}

fn action_kind(action: &ActionKind) -> ResourceKind {
    match action {
        ActionKind::WriteFile { .. }
        | ActionKind::SetFileAttributes { .. }
        | ActionKind::RemoveFile { .. } => ResourceKind::File,
        ActionKind::EnsureDirectory(_) => ResourceKind::Directory,
        ActionKind::EnsureUser(_) | ActionKind::SetUserPrimaryGroup { .. } => ResourceKind::User,
        ActionKind::EnsureGroup(_) => ResourceKind::Group,
        ActionKind::InstallPackage { .. } | ActionKind::RemovePackage { .. } => {
            ResourceKind::Package
        }
        ActionKind::DeployUnit { .. }
        | ActionKind::ReloadManager(_)
        | ActionKind::SetUnitEnabled { .. }
        | ActionKind::SetUnitRunning { .. }
        | ActionKind::NotifyService { .. } => ResourceKind::SystemdUnit,
        ActionKind::UpdateAuthorizedKeys(_) => ResourceKind::AuthorizedKeys,
        ActionKind::DeployAgeSecret(_) | ActionKind::SetAgeSecretAttributes(_) => {
            ResourceKind::AgeSecret
        }
    }
}

/// Load an approved plan under the target state transaction.
pub fn load_plan_for_execution(
    guard: &StateGuard<'_>,
    id: &str,
    binding: PlanBinding<'_>,
) -> Result<LoadedPlan, StateError> {
    match guard.load_plan(id, binding) {
        Ok(plan) => Ok(LoadedPlan::Approved(Box::new(plan))),
        Err(StateError::ExpiredPlan { .. }) => Ok(LoadedPlan::Expired),
        Err(error) => Err(error),
    }
}
