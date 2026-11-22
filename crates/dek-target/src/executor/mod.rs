//! Recheck approved plans and execute them using the target-owned journal.

mod scope;

use std::{
    collections::{BTreeMap, BTreeSet},
    io,
};

use dek_core::{Action, ActionKind, InputObservation, Observation, Plan, ResourceKind, UnitId};
use thiserror::Error;

use crate::{
    backend::{
        ActionContext, ActionReadiness, Backend, BackendError, BackendRegistry, NotificationError,
        PriorAttempt, RecoveryReadiness,
    },
    planner::{self, PlanError},
    state::{
        ActionAttempt, ActionReceipt, ActionResult, PlanBinding, ReviewedPlan, RunAction,
        RunRecord, StateError, StateGuard, StateStore,
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
    CompletedActionDrift,
}

impl std::fmt::Display for ReviewReason {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Expired => out.write_str("已审阅计划过期"),
            Self::InputChanged(input) => write!(out, "已审阅的目标输入 {input} 发生变化"),
            Self::ScopeExpanded => out.write_str("目标实际所需的变更超出已审阅范围"),
            Self::CompletedActionDrift => out.write_str("上次已核验的动作需要重新规划"),
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
    #[error("服务通知 {id}（{unit:?}）：{error}")]
    Notification {
        id: String,
        unit: UnitId,
        #[source]
        error: NotificationError,
    },
    #[error("无法复核目标输入 {input:?}：{message}。请重新规划")]
    Input { input: String, message: String },
    #[error("执行记录 {0} 的动作与已审阅计划不符，请重新规划")]
    RunMismatch(String),
    #[error("请求标识已绑定执行 {id} 的计划 {plan}，请使用原计划或另一个请求标识")]
    RequestMismatch { id: String, plan: String },
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

enum Application<'a> {
    New { request_id: &'a str },
    Resume(Box<RunRecord>),
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
        observe_input: impl FnMut(&InputObservation) -> Result<Observation, String>,
        started: impl FnOnce(&str) -> io::Result<()>,
        load: impl FnOnce(&StateGuard<'_>, &str, PlanBinding<'_>) -> Result<LoadedPlan, StateError>,
    ) -> Result<ExecutionOutcome, ExecutionError> {
        let _lease = self.store.execution_lease()?;
        let previous = self
            .store
            .lock()?
            .run_for_request(request.binding, request.request_id)?;
        if let Some(run) = previous {
            if run.plan_id != request.plan_id {
                return Err(ExecutionError::RequestMismatch {
                    id: run.id,
                    plan: run.plan_id,
                });
            }
            return self.resume_locked(request.binding, run, observe_input, started);
        }
        let loaded = load(&self.store.lock()?, request.plan_id, request.binding)?;
        let LoadedPlan::Approved(reviewed) = loaded else {
            return Ok(ExecutionOutcome::review(ReviewReason::Expired));
        };
        if reviewed.plan.id != request.plan_id {
            return Err(ExecutionError::RunMismatch(request.plan_id.to_owned()));
        }
        self.run_review(
            request.binding,
            &reviewed,
            Application::New {
                request_id: request.request_id,
            },
            observe_input,
            started,
        )
    }

    /// Resume only the bound run. A complete run remains readable after plan GC.
    pub fn resume(
        &self,
        binding: PlanBinding<'_>,
        run_id: &str,
        observe_input: impl FnMut(&InputObservation) -> Result<Observation, String>,
        started: impl FnOnce(&str) -> io::Result<()>,
    ) -> Result<ExecutionOutcome, ExecutionError> {
        let _lease = self.store.execution_lease()?;
        let run = self.store.lock()?.load_run(run_id, binding)?;
        self.resume_locked(binding, run, observe_input, started)
    }

    fn resume_locked(
        &self,
        binding: PlanBinding<'_>,
        run: RunRecord,
        observe_input: impl FnMut(&InputObservation) -> Result<Observation, String>,
        started: impl FnOnce(&str) -> io::Result<()>,
    ) -> Result<ExecutionOutcome, ExecutionError> {
        if run.finished {
            started(&run.id).map_err(ExecutionError::Output)?;
            return Ok(ExecutionOutcome::Finished(Box::new(run)));
        }
        let reviewed = self.store.lock()?.load_run_plan(&run.id, binding)?;
        validate_run(&run, &reviewed)?;
        self.run_review(
            binding,
            &reviewed,
            Application::Resume(Box::new(run)),
            observe_input,
            started,
        )
    }

    fn run_review(
        &self,
        binding: PlanBinding<'_>,
        reviewed: &ReviewedPlan,
        application: Application<'_>,
        mut observe_input: impl FnMut(&InputObservation) -> Result<Observation, String>,
        started: impl FnOnce(&str) -> io::Result<()>,
    ) -> Result<ExecutionOutcome, ExecutionError> {
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
            binding.repository_id,
            binding.logical_host,
            &reviewed.resources,
            &reviewed.plan.used_inputs,
        )?;
        let resumed = match &application {
            Application::Resume(run) => Some(run.as_ref()),
            Application::New { .. } => None,
        };
        let Scope::Reviewed { live, conflict } = match_scope(&reviewed.plan, &fresh, resumed)
        else {
            return self.replacement(binding, reviewed, &fresh, ReviewReason::ScopeExpanded);
        };
        if let Application::Resume(run) = &application
            && run.actions.iter().any(|item| {
                matches!(item.result, ActionResult::Verified { .. })
                    && live.contains(item.action.id.as_str())
            })
        {
            return self.replacement(
                binding,
                reviewed,
                &fresh,
                ReviewReason::CompletedActionDrift,
            );
        }
        for action in &reviewed.plan.actions {
            if let Application::Resume(run) = &application
                && run.actions.iter().any(|item| {
                    item.action.id == action.id
                        && matches!(item.result, ActionResult::Verified { .. })
                })
            {
                continue;
            }
            self.backend(action)?
                .validate_action(action)
                .map_err(|error| ExecutionError::action(action, error.0))?;
        }

        if conflict.is_none() {
            self.validate_batches(reviewed, &live)?;
        }

        let run = match application {
            Application::New { request_id } => {
                let result = self
                    .store
                    .lock()?
                    .begin_run(&reviewed.plan.id, binding, request_id);
                match result {
                    Err(StateError::ExpiredPlan { .. }) => {
                        return Ok(ExecutionOutcome::review(ReviewReason::Expired));
                    }
                    result => result?,
                }
            }
            Application::Resume(run) => *run,
        };
        validate_run(&run, reviewed)?;
        started(&run.id).map_err(ExecutionError::Output)?;
        if let Some(action) = conflict {
            let failure = ExecutionError::action(action, "前置条件已改变，请重新规划");
            let item = run
                .actions
                .iter()
                .find(|item| item.action.id == action.id)
                .ok_or_else(|| ExecutionError::RunMismatch(run.id.clone()))?;
            if item.attempt == 0 {
                let attempt = self.begin(&run, binding, action)?;
                self.record(&run, binding, action, attempt, &Err(&failure))?;
            } else {
                self.store.lock()?.record_recovery_diagnostic(
                    &run.id,
                    binding,
                    &action.id,
                    item.attempt,
                    failure.to_string(),
                )?;
            }
            return Err(failure);
        }

        self.run_actions(&run, binding, reviewed, &live)?;
        let guard = self.store.lock()?;
        guard.finish_run(&run.id, binding)?;
        Ok(ExecutionOutcome::Finished(Box::new(
            guard.load_run(&run.id, binding)?,
        )))
    }

    fn replacement(
        &self,
        binding: PlanBinding<'_>,
        reviewed: &ReviewedPlan,
        fresh: &Plan,
        reason: ReviewReason,
    ) -> Result<ExecutionOutcome, ExecutionError> {
        let stored = self.store.lock()?.save_plan(
            binding.logical_host,
            fresh,
            &reviewed.resources,
            &reviewed.contents,
        )?;
        Ok(ExecutionOutcome::ReviewRequired {
            reason,
            replacement: Some(Box::new(stored)),
        })
    }

    fn validate_batches(
        &self,
        reviewed: &ReviewedPlan,
        live: &BTreeSet<&str>,
    ) -> Result<(), ExecutionError> {
        let mut batches = BTreeMap::<ResourceKind, Vec<_>>::new();
        for action in &reviewed.plan.actions {
            if !live.contains(action.id.as_str()) {
                continue;
            }
            batches
                .entry(action_kind(&action.kind))
                .or_default()
                .push(ActionContext::new(action, &reviewed.contents));
        }
        for contexts in batches.values() {
            if let Some(first) = contexts.first() {
                self.backend(first.action)?
                    .validate_batch(&reviewed.resources, contexts)
                    .map_err(|err| ExecutionError::action(first.action, err.0))?;
            }
        }
        Ok(())
    }

    fn run_actions(
        &self,
        run: &RunRecord,
        binding: PlanBinding<'_>,
        reviewed: &ReviewedPlan,
        live: &BTreeSet<&str>,
    ) -> Result<(), ExecutionError> {
        let mut triggered = BTreeSet::new();
        let mut deferred = BTreeSet::new();
        for item in &run.actions {
            let action = &item.action;
            if let ActionResult::Verified { changed } = item.result {
                if changed && !runtime_content_check(&action.kind) {
                    triggered.insert(action.id.as_str());
                }
                continue;
            }

            let backend = self.backend(action)?;
            match &item.result {
                ActionResult::Pending => {
                    if matches!(action.kind, ActionKind::NotifyService { .. })
                        && !self.notification_triggered(run, binding, action, live, &triggered)?
                    {
                        let attempt = self.begin(run, binding, action)?;
                        self.record(run, binding, action, attempt, &Ok(false))?;
                        continue;
                    }
                }
                ActionResult::Failed { .. } | ActionResult::Unverified => {
                    if !self.recover_attempt(run, binding, item, backend)? {
                        if !runtime_content_check(&action.kind) {
                            triggered.insert(action.id.as_str());
                        }
                        continue;
                    }
                }
                ActionResult::Deferred { .. } => {
                    deferred.insert(action.id.as_str());
                    continue;
                }
                ActionResult::Verified { .. } => unreachable!("verified actions were handled"),
            }
            let deferred_reason = if let Some(dependency) = action
                .depends_on
                .iter()
                .find(|id| deferred.contains(id.as_str()))
            {
                Some(format!("依赖动作 {dependency} 待激活"))
            } else {
                backend
                    .deferred_reason(action)
                    .map_err(|error| ExecutionError::action(action, error.0))?
            };
            if let Some(message) = deferred_reason {
                self.store
                    .lock()?
                    .defer_action(&run.id, binding, &action.id, message)?;
                deferred.insert(action.id.as_str());
                continue;
            }
            let readiness = backend
                .inspect_action(action)
                .map_err(|error| ExecutionError::action(action, error.0));
            // File writes excluded from the fresh scope did not reach batch validation.
            let readiness = if matches!(readiness, Ok(ActionReadiness::Ready))
                && matches!(action.kind, ActionKind::WriteFile { .. })
                && !live.contains(action.id.as_str())
            {
                Ok(ActionReadiness::Conflict(
                    "目标文件在范围复核后发生变化，请重新规划".into(),
                ))
            } else {
                readiness
            };
            let readiness = if matches!(action.kind, ActionKind::NotifyService { .. }) {
                match readiness? {
                    ActionReadiness::Conflict(reason) => {
                        return Err(ExecutionError::action(action, reason));
                    }
                    ActionReadiness::Ready => {
                        self.verify_notification_files(run, action)?;
                        Ok(ActionReadiness::Ready)
                    }
                    ActionReadiness::Satisfied => Ok(ActionReadiness::Satisfied),
                }
            } else {
                readiness
            };
            let attempt = self.begin(run, binding, action)?;
            let outcome = self.dispatch(run, binding, reviewed, action, backend, readiness);
            self.record(run, binding, action, attempt, &outcome.as_ref().copied())?;
            if outcome? && !runtime_content_check(&action.kind) {
                triggered.insert(action.id.as_str());
            }
        }
        Ok(())
    }

    fn recover_attempt(
        &self,
        run: &RunRecord,
        binding: PlanBinding<'_>,
        item: &RunAction,
        backend: &dyn Backend,
    ) -> Result<bool, ExecutionError> {
        let action = &item.action;
        let prior = match item.result {
            ActionResult::Unverified => PriorAttempt::Unverified,
            ActionResult::Failed {
                notification_job_failed: true,
                ..
            } => PriorAttempt::NotificationJobFailed,
            _ => PriorAttempt::Failed,
        };
        let unverified = prior == PriorAttempt::Unverified;
        let readiness = backend
            .inspect_recovery(action, prior)
            .unwrap_or_else(|error| {
                RecoveryReadiness::Indeterminate(format!("无法观察先前尝试：{}", error.0))
            });
        match readiness {
            RecoveryReadiness::Satisfied => {
                let attempt = if unverified {
                    item.attempt
                } else {
                    self.begin(run, binding, action)?
                };
                self.record(run, binding, action, attempt, &Ok(false))?;
                Ok(false)
            }
            RecoveryReadiness::Retry => {
                if unverified {
                    self.store.lock()?.record_action_result(
                        &run.id,
                        binding,
                        &action.id,
                        item.attempt,
                        ActionReceipt::Failed {
                            message: "复查显示该动作仍需执行，前置条件允许续办".into(),
                        },
                    )?;
                }
                Ok(true)
            }
            RecoveryReadiness::Conflict(reason) | RecoveryReadiness::Indeterminate(reason) => {
                self.store.lock()?.record_recovery_diagnostic(
                    &run.id,
                    binding,
                    &action.id,
                    item.attempt,
                    reason.clone(),
                )?;
                Err(ExecutionError::action(action, reason))
            }
        }
    }

    fn begin(
        &self,
        run: &RunRecord,
        binding: PlanBinding<'_>,
        action: &Action,
    ) -> Result<u32, ExecutionError> {
        match self
            .store
            .lock()?
            .begin_action(&run.id, binding, &action.id)?
        {
            ActionAttempt::Started(attempt) => Ok(attempt),
            ActionAttempt::NeedsVerification(_) => {
                Err(ExecutionError::action(action, "先前尝试的结果仍待核验"))
            }
        }
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

        let queue = || {
            self.queue_dependent_notifications(run, binding, action)
                .map_err(|error| BackendError(error.to_string()))
        };
        let context = ActionContext::new(action, &reviewed.contents);
        let context = if runtime_content_check(&action.kind) {
            context.with_content_change(&queue)
        } else {
            self.queue_dependent_notifications(run, binding, action)?;
            context
        };
        if let ActionKind::NotifyService { unit, .. } = &action.kind {
            return backend
                .execute_notification(&context)
                .and_then(|()| verify(backend, action).map_err(NotificationError::Unverified))
                .map(|()| true)
                .map_err(|error| ExecutionError::Notification {
                    id: action.id.clone(),
                    unit: unit.clone(),
                    error,
                });
        }
        backend
            .execute_changed(&context)
            .and_then(|changed| verify(backend, action).map(|()| changed))
            .map_err(|error| ExecutionError::action(action, error.0))
    }

    fn queue_dependent_notifications(
        &self,
        run: &RunRecord,
        binding: PlanBinding<'_>,
        action: &Action,
    ) -> Result<(), StateError> {
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

        Ok(())
    }

    fn notification_triggered(
        &self,
        run: &RunRecord,
        binding: PlanBinding<'_>,
        notice: &Action,
        live: &BTreeSet<&str>,
        triggered: &BTreeSet<&str>,
    ) -> Result<bool, StateError> {
        if notice
            .depends_on
            .iter()
            .any(|id| triggered.contains(id.as_str()))
        {
            return Ok(true);
        }
        let runtime = run.actions.iter().any(|item| {
            runtime_content_check(&item.action.kind) && notice.depends_on.contains(&item.action.id)
        });
        let pending = if runtime {
            // Runtime checks may have queued this notice after this run was loaded.
            self.store
                .lock()?
                .load_run(&run.id, binding)?
                .pending_notifications
        } else {
            if live.contains(notice.id.as_str()) {
                return Ok(true);
            }
            return Ok(run
                .pending_notifications
                .iter()
                .any(|pending| pending.action_id == notice.id));
        };
        Ok(pending.iter().any(|pending| pending.action_id == notice.id))
    }

    fn verify_notification_files(
        &self,
        run: &RunRecord,
        notice: &Action,
    ) -> Result<(), ExecutionError> {
        for item in &run.actions {
            let source = &item.action;
            if matches!(
                action_kind(&source.kind),
                ResourceKind::File | ResourceKind::AgeSecret
            ) && notice.depends_on.contains(&source.id)
                && !self
                    .backend(source)?
                    .verify(&source.kind)
                    .map_err(|error| ExecutionError::action(source, error.0))?
            {
                return Err(ExecutionError::action(
                    source,
                    "通知前复核发现文件与已审阅状态不符",
                ));
            }
        }
        Ok(())
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
            Err(
                error @ ExecutionError::Notification {
                    error: NotificationError::JobFailed(_),
                    ..
                },
            ) => ActionReceipt::NotificationJobFailed {
                message: error.to_string(),
            },
            Err(
                error @ ExecutionError::Notification {
                    error: NotificationError::Unverified(_),
                    ..
                },
            ) => {
                return self.store.lock()?.record_recovery_diagnostic(
                    &run.id,
                    binding,
                    &action.id,
                    attempt,
                    error.to_string(),
                );
            }
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

fn verify(backend: &dyn Backend, action: &Action) -> Result<(), BackendError> {
    if backend.verify(&action.kind)? {
        Ok(())
    } else {
        Err(BackendError("执行后的状态核验未通过".into()))
    }
}

fn validate_run(run: &RunRecord, reviewed: &ReviewedPlan) -> Result<(), ExecutionError> {
    if run
        .actions
        .iter()
        .map(|item| &item.action)
        .eq(&reviewed.plan.actions)
    {
        Ok(())
    } else {
        Err(ExecutionError::RunMismatch(run.id.clone()))
    }
}

fn runtime_content_check(kind: &ActionKind) -> bool {
    matches!(
        kind,
        ActionKind::DeployAgeSecret(_) | ActionKind::SetAgeSecretAttributes(_)
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
