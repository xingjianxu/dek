use std::{
    io::Cursor,
    time::{SystemTime, UNIX_EPOCH},
};

use dek_core::{Action, ActionKind, ServiceEffect, TargetIdentity, UnitId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{PlanBinding, StateError, StateGuard, fs::Directory, valid_hash};

const RUN_FORMAT_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunRecord {
    format_version: u32,
    pub id: String,
    pub plan_id: String,
    pub repository_id: String,
    pub logical_host: String,
    pub target: TargetIdentity,
    pub request_id: String,
    pub created_at: u64,
    sequence: u64,
    pub actions: Vec<RunAction>,
    pub pending_notifications: Vec<RunNotification>,
    pub finished: bool,
}

impl RunRecord {
    fn binding(&self) -> PlanBinding<'_> {
        PlanBinding {
            repository_id: &self.repository_id,
            logical_host: &self.logical_host,
            target: &self.target,
        }
    }

    fn matches(&self, binding: PlanBinding<'_>) -> bool {
        self.repository_id == binding.repository_id
            && self.logical_host == binding.logical_host
            && self.target == *binding.target
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunAction {
    pub action: Action,
    pub attempt: u32,
    pub result: ActionResult,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_diagnostic: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum ActionResult {
    Pending,
    Unverified,
    Deferred { message: String },
    Verified { changed: bool },
    Failed { message: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActionAttempt {
    /// A new attempt is persisted. The backend may now be called.
    Started(u32),
    /// A previous attempt needs observation before the backend can be called again.
    NeedsVerification(u32),
}

pub enum ActionReceipt {
    Verified { changed: bool },
    Failed { message: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunNotification {
    pub action_id: String,
    pub unit: UnitId,
    pub effect: ServiceEffect,
    pub depends_on: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingRunNotification {
    pub run_id: String,
    pub notification: RunNotification,
    pub result: ActionResult,
    pub prerequisites_verified: bool,
}

impl StateGuard<'_> {
    /// A retry with the same request ID returns the original record and cannot
    /// substitute a different plan. Creating a run pins its verified plan.
    pub fn begin_run(
        &self,
        plan_id: &str,
        binding: PlanBinding<'_>,
        request_id: &str,
    ) -> Result<RunRecord, StateError> {
        let runs = self.runs_directory()?;
        if request_id.trim().is_empty() {
            return Err(StateError::invalid(&runs.path, "请求标识不能为空"));
        }
        let id = run_id(binding, request_id);
        let name = run_filename(&runs, &id)?;
        if let Some(existing) = read_run(&runs, &name)? {
            if !existing.matches(binding) || existing.plan_id != plan_id {
                return Err(StateError::invalid(
                    &runs.path.join(name),
                    "相同请求标识已绑定另一个目标或计划",
                ));
            }
            runs.sync()?;
            return Ok(existing);
        }

        let reviewed = self.load_plan(plan_id, binding)?;
        let sequence = match latest(&runs, binding)? {
            Some(previous) => previous
                .sequence
                .checked_add(1)
                .ok_or_else(|| StateError::invalid(&runs.path, "运行记录序号已达上限"))?,
            None => 1,
        };
        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let run = RunRecord {
            format_version: RUN_FORMAT_VERSION,
            id,
            plan_id: plan_id.into(),
            repository_id: binding.repository_id.into(),
            logical_host: binding.logical_host.into(),
            target: binding.target.clone(),
            request_id: request_id.into(),
            sequence,
            created_at,
            actions: reviewed
                .plan
                .actions
                .into_iter()
                .map(|action| RunAction {
                    action,
                    attempt: 0,
                    result: ActionResult::Pending,
                    recovery_diagnostic: None,
                })
                .collect(),
            pending_notifications: Vec::new(),
            finished: false,
        };
        let bytes = encode(&run, &runs)?;
        runs.publish(&name, &bytes)?;
        Ok(run)
    }

    pub fn load_run(&self, id: &str, binding: PlanBinding<'_>) -> Result<RunRecord, StateError> {
        let runs = self.runs_directory()?;
        bound_run(&runs, id, binding)
    }

    pub fn latest_run(&self, binding: PlanBinding<'_>) -> Result<Option<RunRecord>, StateError> {
        latest(&self.runs_directory()?, binding)
    }

    /// Resolve a replay before loading the review, which may have been collected.
    pub fn run_for_request(
        &self,
        binding: PlanBinding<'_>,
        request_id: &str,
    ) -> Result<Option<RunRecord>, StateError> {
        let runs = self.runs_directory()?;
        let id = run_id(binding, request_id);
        let name = run_filename(&runs, &id)?;
        let run = read_run(&runs, &name)?;
        if let Some(run) = &run
            && (!run.matches(binding) || run.request_id != request_id)
        {
            return Err(StateError::invalid(
                &runs.path.join(name),
                "执行请求标识的归属不符",
            ));
        }
        Ok(run)
    }

    /// Expose queued notices and their dependency state to later planners.
    pub fn pending_notifications(
        &self,
        binding: PlanBinding<'_>,
    ) -> Result<Vec<PendingRunNotification>, StateError> {
        let mut notices = Vec::new();
        for run in self
            .active_runs()?
            .into_iter()
            .filter(|run| run.matches(binding))
        {
            for notification in run.pending_notifications {
                let Some(action) = run
                    .actions
                    .iter()
                    .find(|item| item.action.id == notification.action_id)
                else {
                    return Err(StateError::invalid(
                        &self.store.root.path,
                        "执行记录的待重试通知无对应动作",
                    ));
                };
                let prerequisites_verified = notification.depends_on.iter().all(|dependency| {
                    run.actions.iter().any(|item| {
                        item.action.id == *dependency
                            && matches!(item.result, ActionResult::Verified { .. })
                    })
                });
                notices.push(PendingRunNotification {
                    run_id: run.id.clone(),
                    notification,
                    result: action.result.clone(),
                    prerequisites_verified,
                });
            }
        }
        notices.sort_by(|a, b| {
            (&a.run_id, &a.notification.action_id).cmp(&(&b.run_id, &b.notification.action_id))
        });
        Ok(notices)
    }

    /// Keep a failed or unverified attempt inspectable without certifying it.
    pub fn record_recovery_diagnostic(
        &self,
        run_id: &str,
        binding: PlanBinding<'_>,
        action_id: &str,
        attempt: u32,
        diagnostic: String,
    ) -> Result<(), StateError> {
        let runs = self.runs_directory()?;
        let mut run = bound_run(&runs, run_id, binding)?;
        let action = action_mut(&runs, &mut run, action_id)?;
        if action.attempt != attempt
            || !matches!(
                action.result,
                ActionResult::Failed { .. } | ActionResult::Unverified
            )
        {
            return Err(StateError::invalid(&runs.path, "恢复诊断与当前尝试不符"));
        }
        if action.recovery_diagnostic.as_deref() == Some(&diagnostic) {
            return runs.sync();
        }
        action.recovery_diagnostic = Some(diagnostic);
        replace_run(&runs, &run)
    }

    /// Publish the unknown-result state before calling the backend.
    pub fn begin_action(
        &self,
        run_id: &str,
        binding: PlanBinding<'_>,
        action_id: &str,
    ) -> Result<ActionAttempt, StateError> {
        let runs = self.runs_directory()?;
        let mut run = bound_run(&runs, run_id, binding)?;
        let action = action_mut(&runs, &mut run, action_id)?;
        match action.result {
            ActionResult::Unverified => {
                runs.sync()?;
                return Ok(ActionAttempt::NeedsVerification(action.attempt));
            }
            ActionResult::Verified { .. } | ActionResult::Deferred { .. } => {
                return Err(StateError::invalid(
                    &runs.path,
                    format!("动作 {action_id} 已核验完成"),
                ));
            }
            ActionResult::Pending | ActionResult::Failed { .. } => {}
        }
        action.attempt = action.attempt.checked_add(1).ok_or_else(|| {
            StateError::invalid(&runs.path, format!("动作 {action_id} 尝试次数已达上限"))
        })?;
        action.result = ActionResult::Unverified;
        action.recovery_diagnostic = None;
        let attempt = action.attempt;
        replace_run(&runs, &run)?;
        Ok(ActionAttempt::Started(attempt))
    }

    /// Record an action that requires a live manager without starting an attempt.
    pub(crate) fn defer_action(
        &self,
        run_id: &str,
        binding: PlanBinding<'_>,
        action_id: &str,
        message: String,
    ) -> Result<(), StateError> {
        let runs = self.runs_directory()?;
        let mut run = bound_run(&runs, run_id, binding)?;
        let action = action_mut(&runs, &mut run, action_id)?;
        if !matches!(
            action.result,
            ActionResult::Pending | ActionResult::Failed { .. }
        ) {
            return Err(StateError::invalid(
                &runs.path,
                "仅可延后未开始的动作或明确失败的动作",
            ));
        }
        action.result = ActionResult::Deferred { message };
        action.recovery_diagnostic = None;
        replace_run(&runs, &run)
    }

    pub fn record_action_result(
        &self,
        run_id: &str,
        binding: PlanBinding<'_>,
        action_id: &str,
        attempt: u32,
        receipt: ActionReceipt,
    ) -> Result<(), StateError> {
        let runs = self.runs_directory()?;
        let mut run = bound_run(&runs, run_id, binding)?;
        let action = action_mut(&runs, &mut run, action_id)?;
        let next = match receipt {
            ActionReceipt::Verified { changed } => ActionResult::Verified { changed },
            ActionReceipt::Failed { message } => ActionResult::Failed { message },
        };
        if attempt != action.attempt || attempt == 0 {
            return Err(StateError::invalid(
                &runs.path,
                format!("动作 {action_id} 的回执尝试号不符"),
            ));
        }
        if action.result == next {
            return runs.sync();
        }
        if action.result != ActionResult::Unverified {
            return Err(StateError::invalid(
                &runs.path,
                format!("动作 {action_id} 已有其他回执"),
            ));
        }
        let completed = matches!(next, ActionResult::Verified { .. });
        action.result = next;
        action.recovery_diagnostic = None;
        if completed {
            run.pending_notifications
                .retain(|notice| notice.action_id != action_id);
        }
        replace_run(&runs, &run)
    }

    /// Pre-register the notification before applying a file that may trigger it.
    pub fn queue_notification(
        &self,
        run_id: &str,
        binding: PlanBinding<'_>,
        action_id: &str,
    ) -> Result<(), StateError> {
        let runs = self.runs_directory()?;
        let mut run = bound_run(&runs, run_id, binding)?;
        if run
            .pending_notifications
            .iter()
            .any(|notice| notice.action_id == action_id)
        {
            return runs.sync();
        }
        let action = action_mut(&runs, &mut run, action_id)?;
        let ActionKind::NotifyService { unit, effect } = &action.action.kind else {
            return Err(StateError::invalid(
                &runs.path,
                format!("动作 {action_id} 不是服务通知"),
            ));
        };
        if matches!(action.result, ActionResult::Verified { .. }) {
            return runs.sync();
        }
        let notice = RunNotification {
            action_id: action_id.into(),
            unit: unit.clone(),
            effect: *effect,
            depends_on: action.action.depends_on.clone(),
        };
        run.pending_notifications.push(notice);
        replace_run(&runs, &run)
    }

    /// Mark a run complete after verifying or explicitly deferring every action.
    pub fn finish_run(&self, id: &str, binding: PlanBinding<'_>) -> Result<(), StateError> {
        let runs = self.runs_directory()?;
        let mut run = bound_run(&runs, id, binding)?;
        if run.finished {
            return runs.sync();
        }
        if !run.pending_notifications.is_empty()
            || run.actions.iter().any(|action| {
                !matches!(
                    action.result,
                    ActionResult::Verified { .. } | ActionResult::Deferred { .. }
                )
            })
        {
            return Err(StateError::invalid(
                &runs.path.join(run_filename(&runs, id)?),
                "仍有未核验动作或服务通知",
            ));
        }
        run.finished = true;
        replace_run(&runs, &run)
    }

    pub(super) fn active_runs(&self) -> Result<Vec<RunRecord>, StateError> {
        let mut active = Vec::new();
        let Some(runs) = self.store.root.existing_private_child("runs")? else {
            return Ok(active);
        };
        for name in runs.file_names()? {
            if name.ends_with(".mpk")
                && let Some(record) = read_run(&runs, &name)?
                && !record.finished
            {
                active.push(record);
            }
        }
        Ok(active)
    }

    fn runs_directory(&self) -> Result<Directory, StateError> {
        self.store.root.private_child("runs")
    }
}

fn run_id(binding: PlanBinding<'_>, request_id: &str) -> String {
    let input = (
        "dek-run-v1",
        binding.repository_id,
        binding.logical_host,
        binding.target,
        request_id,
    );
    let bytes = serde_json::to_vec(&input).expect("run identity only contains serializable fields");
    format!("{:x}", Sha256::digest(bytes))
}

fn run_filename(runs: &Directory, id: &str) -> Result<String, StateError> {
    if !valid_hash(id) {
        return Err(StateError::invalid(
            &runs.path,
            format!("无效执行 ID {id:?}"),
        ));
    }
    Ok(format!("{id}.mpk"))
}

fn read_run(runs: &Directory, name: &str) -> Result<Option<RunRecord>, StateError> {
    let Some(bytes) = runs.read(name)? else {
        return Ok(None);
    };
    let path = runs.path.join(name);
    let mut cursor = Cursor::new(&bytes);
    let record: RunRecord = rmp_serde::from_read(&mut cursor)
        .map_err(|err| StateError::invalid(&path, format!("无法解码执行记录：{err}")))?;
    if cursor.position() != bytes.len() as u64
        || record.format_version != RUN_FORMAT_VERSION
        || !valid_hash(&record.plan_id)
        || record.id != run_id(record.binding(), &record.request_id)
        || run_filename(runs, &record.id)? != name
    {
        return Err(StateError::invalid(&path, "执行记录的版本、引用或标识无效"));
    }
    Ok(Some(record))
}

fn bound_run(
    runs: &Directory,
    id: &str,
    binding: PlanBinding<'_>,
) -> Result<RunRecord, StateError> {
    let name = run_filename(runs, id)?;
    let path = runs.path.join(&name);
    let run = read_run(runs, &name)?.ok_or_else(|| StateError::invalid(&path, "找不到执行记录"))?;
    if !run.matches(binding) {
        return Err(StateError::invalid(&path, "执行记录属于另一仓库或目标主机"));
    }
    Ok(run)
}

fn latest(runs: &Directory, binding: PlanBinding<'_>) -> Result<Option<RunRecord>, StateError> {
    let mut best: Option<RunRecord> = None;
    for name in runs.file_names()? {
        if !name.ends_with(".mpk") {
            continue;
        }
        if let Some(record) = read_run(runs, &name)?
            && record.matches(binding)
            && best
                .as_ref()
                .is_none_or(|previous| record.sequence > previous.sequence)
        {
            best = Some(record);
        }
    }
    Ok(best)
}

fn encode(run: &RunRecord, runs: &Directory) -> Result<Vec<u8>, StateError> {
    rmp_serde::to_vec_named(run)
        .map_err(|err| StateError::invalid(&runs.path, format!("无法编码执行记录：{err}")))
}

fn replace_run(runs: &Directory, run: &RunRecord) -> Result<(), StateError> {
    runs.replace(&run_filename(runs, &run.id)?, &encode(run, runs)?)
}

fn action_mut<'a>(
    runs: &Directory,
    run: &'a mut RunRecord,
    id: &str,
) -> Result<&'a mut RunAction, StateError> {
    run.actions
        .iter_mut()
        .find(|action| action.action.id == id)
        .ok_or_else(|| StateError::invalid(&runs.path, format!("执行记录没有动作 {id:?}")))
}

#[cfg(test)]
mod tests;
