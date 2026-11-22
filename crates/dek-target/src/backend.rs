//! Interfaces for target capabilities and resource backends.

use std::collections::{BTreeMap, BTreeSet};

use dek_core::{
    Action, ActionKind, ContentDigest, DesiredResource, PlanMode, Precondition, ResourceCapability,
    ResourceId, ResourceKind, ResourceSet, ServiceNotification, TargetCapabilities, TargetIdentity,
};
use thiserror::Error;

#[cfg(target_os = "linux")]
use crate::terminal::{TERMINAL_REQUIRED, TerminalCommand, TerminalRunner};

#[derive(Debug, Error)]
#[error("{0}")]
pub struct BackendError(pub String);

#[derive(Debug)]
pub struct PlannedAction {
    pub kind: ActionKind,
    pub reason: String,
    pub preconditions: Vec<Precondition>,
    /// Whether this action would change the resource's content.
    pub content_changed: bool,
}

/// A native package transaction and resource-level plans derived from one snapshot.
#[derive(Debug, Default)]
pub struct PlannedPackages {
    pub transaction: Option<PlannedAction>,
    pub covered: BTreeSet<ResourceId>,
    pub actions: BTreeMap<ResourceId, Vec<PlannedAction>>,
}

#[derive(Debug)]
pub struct PlannedNotification {
    pub reason: String,
    pub preconditions: Vec<Precondition>,
}

pub type RepositoryCommit = Box<dyn FnOnce() -> Result<(), BackendError>>;

/// A native transaction held ready under its database lock, or evidence of drift.
pub enum RepositoryPreparation {
    Ready(RepositoryCommit),
    Changed,
}

/// The current disposition of a reviewed action against live target state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActionReadiness {
    Ready,
    Satisfied,
    Conflict(String),
}

/// State of the previous execution attempt before a recovery observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PriorAttempt {
    Failed,
    NotificationJobFailed,
    Unverified,
}

/// Conclusions the backend can support from a recovery observation.
#[derive(Debug, Eq, PartialEq)]
pub enum RecoveryReadiness {
    Satisfied,
    Retry,
    Conflict(String),
    Indeterminate(String),
}

/// Disposition of a service job reported by its manager.
#[derive(Debug, Error)]
pub enum NotificationError {
    #[error(transparent)]
    JobFailed(BackendError),
    #[error(transparent)]
    Unverified(BackendError),
}

/// A reviewed action and the verified content available to its backend.
pub struct ActionContext<'a> {
    pub action: &'a Action,
    contents: &'a BTreeMap<ContentDigest, Vec<u8>>,
    #[cfg(target_os = "linux")]
    terminal: Option<&'a TerminalRunner<'a>>,
    before_content_change: Option<&'a (dyn Fn(bool) -> Result<(), BackendError> + Sync)>,
}

/// Inputs available to backends that need reviewed bytes while planning.
pub struct PlanningContext<'a> {
    pub resources: &'a ResourceSet,
    pub(crate) contents: &'a BTreeMap<ContentDigest, Vec<u8>>,
}

impl PlanningContext<'_> {
    pub fn content(&self, digest: &ContentDigest) -> Result<&[u8], BackendError> {
        self.contents
            .get(digest)
            .map(Vec::as_slice)
            .ok_or_else(|| BackendError(format!("缺少已审阅内容 {}", digest.0)))
    }
}

impl<'a> ActionContext<'a> {
    #[cfg(target_os = "linux")]
    pub(crate) fn new(action: &'a Action, contents: &'a BTreeMap<ContentDigest, Vec<u8>>) -> Self {
        Self {
            action,
            contents,
            terminal: None,
            before_content_change: None,
        }
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn with_terminal(mut self, terminal: Option<&'a TerminalRunner<'a>>) -> Self {
        self.terminal = terminal;
        self
    }

    /// Run a helper using the approved action's interactive terminal.
    #[cfg(target_os = "linux")]
    pub fn run_interactive(&self, command: &TerminalCommand) -> Result<(), BackendError> {
        let terminal = self
            .terminal
            .ok_or_else(|| BackendError(TERMINAL_REQUIRED.into()))?;
        terminal(command)
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn with_content_change(
        mut self,
        before: &'a (dyn Fn(bool) -> Result<(), BackendError> + Sync),
    ) -> Self {
        self.before_content_change = Some(before);
        self
    }

    /// Persist dependent notices immediately before publishing verified changed content.
    pub fn before_content_change(&self) -> Result<(), BackendError> {
        self.before_file_change(true)
    }

    /// Persist only notices that explicitly cover a metadata change.
    pub fn before_metadata_change(&self) -> Result<(), BackendError> {
        self.before_file_change(false)
    }

    fn before_file_change(&self, content_changed: bool) -> Result<(), BackendError> {
        match self.before_content_change {
            Some(before) => before(content_changed),
            None => Ok(()),
        }
    }

    pub fn content(&self, digest: &ContentDigest) -> Result<&[u8], BackendError> {
        self.contents
            .get(digest)
            .map(Vec::as_slice)
            .ok_or_else(|| BackendError(format!("缺少已审阅内容 {}", digest.0)))
    }
}

/// A resource backend supplies read-only planning operations and execution hooks.
pub trait Backend {
    fn capabilities(&self) -> Vec<ResourceCapability>;

    /// A backend may explicitly support plans issued with an older version.
    fn compatible_version(&self, current: &ResourceCapability, planned: u32) -> bool {
        current.version == planned
    }

    fn validate(&self, resource: &DesiredResource) -> Result<(), BackendError>;

    /// Protect paths referenced by this resource from conflicting managed resources.
    fn validate_managed_paths(
        &self,
        _resource: &DesiredResource,
        _managed: &[ResourceId],
    ) -> Result<(), BackendError> {
        Ok(())
    }

    /// Validate related declarations and report prerequisites inferred on this target.
    fn resource_dependencies(
        &self,
        _resource: &DesiredResource,
        _managed: &ResourceSet,
    ) -> Result<Vec<ResourceId>, BackendError> {
        Ok(Vec::new())
    }

    /// Observe actual state in the same typed form used for reviewed preconditions.
    fn observe(&self, resource: &DesiredResource) -> Result<Vec<Precondition>, BackendError>;

    /// Return the actions that would change this resource, in dependency order.
    fn plan(
        &self,
        resource: &DesiredResource,
        observed: &[Precondition],
    ) -> Result<Vec<PlannedAction>, BackendError>;

    fn plan_with_contents(
        &self,
        resource: &DesiredResource,
        _: &PlanningContext<'_>,
    ) -> Result<Vec<PlannedAction>, BackendError> {
        let observed = self
            .observe(resource)
            .map_err(|err| BackendError(format!("observe: {err}")))?;
        self.plan(resource, &observed)
            .map_err(|err| BackendError(format!("plan: {err}")))
    }

    /// Plan package work that must be resolved across several resource declarations.
    fn plan_packages(
        &self,
        _resources: &ResourceSet,
        _mode: PlanMode,
        _reviewed: Option<&[Action]>,
    ) -> Result<Option<PlannedPackages>, BackendError> {
        Ok(None)
    }

    fn validate_notification(&self, _: &ServiceNotification) -> Result<(), BackendError> {
        Ok(())
    }

    /// Determine whether the service needs notification and capture its preconditions.
    /// When `service_will_start` is true, the notification follows a planned start.
    fn plan_notification(
        &self,
        _: &ServiceNotification,
        _service_will_start: bool,
    ) -> Result<Option<PlannedNotification>, BackendError> {
        Err(BackendError(
            "backend does not plan service notifications".into(),
        ))
    }

    /// Validate execution support without observing or mutating the resource.
    fn validate_action(&self, _: &Action) -> Result<(), BackendError> {
        Err(BackendError("后端未提供此动作的执行能力".into()))
    }

    /// Prepare the reviewed repository transaction at its native mutation boundary.
    fn prepare_repository_transaction(
        &self,
        _: &Action,
    ) -> Result<RepositoryPreparation, BackendError> {
        Err(BackendError("后端未提供仓库事务的准备能力".into()))
    }

    /// Validate reviewed inputs that must be checked together before any action runs.
    fn validate_batch(&self, _: &ResourceSet, _: &[ActionContext<'_>]) -> Result<(), BackendError> {
        Ok(())
    }

    /// Compare the action goal and reviewed preconditions with live state.
    /// Return Satisfied only for effects that the backend can actually verify.
    fn inspect_action(&self, _: &Action) -> Result<ActionReadiness, BackendError> {
        Err(BackendError("后端未提供此动作的前置条件核对".into()))
    }

    /// Explain why a reviewed action must wait for an unavailable manager.
    fn deferred_reason(&self, _: &Action) -> Result<Option<String>, BackendError> {
        Ok(None)
    }

    /// Reobserve a recorded attempt. Backends may override this when they can
    /// independently determine the outcome of a package transaction or effect.
    fn inspect_recovery(
        &self,
        action: &Action,
        prior: PriorAttempt,
    ) -> Result<RecoveryReadiness, BackendError> {
        let ambiguous = match action.kind {
            ActionKind::InstallPackage { .. }
            | ActionKind::RemovePackage { .. }
            | ActionKind::RepositoryTransaction(_)
            | ActionKind::ReloadManager(_) => true,
            ActionKind::NotifyService { .. } => prior != PriorAttempt::NotificationJobFailed,
            _ => false,
        };
        Ok(match self.inspect_action(action)? {
            ActionReadiness::Conflict(reason) => RecoveryReadiness::Conflict(reason),
            ActionReadiness::Satisfied
                if matches!(action.kind, ActionKind::RepositoryTransaction(_)) =>
            {
                RecoveryReadiness::Satisfied
            }
            _ if ambiguous => RecoveryReadiness::Indeterminate(
                "无法从当前状态确认包事务或服务动作的结果，请在目标机查核后查看 dek status".into(),
            ),
            ActionReadiness::Ready => RecoveryReadiness::Retry,
            ActionReadiness::Satisfied => RecoveryReadiness::Satisfied,
        })
    }

    /// Declare a helper's TTY requirement before the executor records an attempt.
    #[cfg(target_os = "linux")]
    fn requires_terminal(&self, _: &Action) -> bool {
        false
    }

    /// Apply the reviewed action. Backends check the supplied preconditions at
    /// their mutation boundary and use only the reviewed bytes from context.
    fn execute(&self, context: &ActionContext<'_>) -> Result<(), BackendError>;

    /// Whether the action made a visible change after execution.
    fn execute_changed(&self, context: &ActionContext<'_>) -> Result<bool, BackendError> {
        self.execute(context).map(|()| true)
    }

    /// Return a retryable failure only with a definitive service job receipt.
    fn execute_notification(&self, context: &ActionContext<'_>) -> Result<(), NotificationError> {
        self.execute(context).map_err(NotificationError::Unverified)
    }

    /// Observe the result after the execution hook succeeds.
    fn verify(&self, action: &ActionKind) -> Result<bool, BackendError>;
}

#[derive(Debug, Error)]
#[error("target {target}: more than one backend capability registered for {kind:?}")]
pub struct DuplicateCapability {
    pub target: String,
    pub kind: ResourceKind,
}

type NormalizePath<'a> = dyn Fn(&str) -> Result<String, BackendError> + 'a;

pub struct BackendRegistry<'a> {
    target: TargetIdentity,
    protocol_version: u32,
    normalize_path: Box<NormalizePath<'a>>,
    backends: BTreeMap<ResourceKind, (ResourceCapability, &'a dyn Backend)>,
}

impl<'a> BackendRegistry<'a> {
    /// The normalizer returns an identity key for a raw target path. All
    /// registered backends share it to detect aliases across resource kinds.
    pub fn new(
        target: TargetIdentity,
        protocol_version: u32,
        normalize_path: impl Fn(&str) -> Result<String, BackendError> + 'a,
    ) -> Self {
        Self {
            target,
            protocol_version,
            normalize_path: Box::new(normalize_path),
            backends: BTreeMap::new(),
        }
    }

    pub fn register(&mut self, backend: &'a dyn Backend) -> Result<(), DuplicateCapability> {
        let mut pending = BTreeMap::new();
        for capability in backend.capabilities() {
            let kind = capability.kind;
            if self.backends.contains_key(&kind)
                || pending.insert(kind, (capability, backend)).is_some()
            {
                return Err(DuplicateCapability {
                    target: self.target.id.clone(),
                    kind,
                });
            }
        }
        self.backends.extend(pending);
        Ok(())
    }

    pub fn target(&self) -> &TargetIdentity {
        &self.target
    }

    pub fn capabilities(&self) -> TargetCapabilities {
        TargetCapabilities {
            target: self.target.clone(),
            protocol_version: self.protocol_version,
            resources: self
                .backends
                .values()
                .map(|(capability, _)| capability.clone())
                .collect(),
        }
    }

    pub fn backend(&self, kind: ResourceKind) -> Option<(&ResourceCapability, &dyn Backend)> {
        self.backends
            .get(&kind)
            .map(|(capability, backend)| (capability, *backend))
    }

    pub(crate) fn normalize_path(&self, path: &str) -> Result<String, BackendError> {
        (self.normalize_path)(path)
    }
}
