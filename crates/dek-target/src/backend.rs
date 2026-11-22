//! Interfaces for target capabilities and resource backends.

use std::collections::BTreeMap;

use dek_core::{
    Action, ActionKind, ContentDigest, DesiredResource, Precondition, ResourceCapability,
    ResourceId, ResourceKind, ResourceSet, ServiceNotification, TargetCapabilities, TargetIdentity,
};
use thiserror::Error;

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

#[derive(Debug)]
pub struct PlannedNotification {
    pub reason: String,
    pub preconditions: Vec<Precondition>,
}

/// The current disposition of a reviewed action against live target state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActionReadiness {
    Ready,
    Satisfied,
    Conflict(String),
}

/// A reviewed action and the verified content available to its backend.
pub struct ActionContext<'a> {
    pub action: &'a Action,
    contents: &'a BTreeMap<ContentDigest, Vec<u8>>,
}

impl<'a> ActionContext<'a> {
    #[cfg(target_os = "linux")]
    pub(crate) fn new(action: &'a Action, contents: &'a BTreeMap<ContentDigest, Vec<u8>>) -> Self {
        Self { action, contents }
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

    /// Compare the action goal and reviewed preconditions with live state.
    /// Return Satisfied only for effects that the backend can actually verify.
    fn inspect_action(&self, _: &Action) -> Result<ActionReadiness, BackendError> {
        Err(BackendError("后端未提供此动作的前置条件核对".into()))
    }

    /// Apply the reviewed action. Backends check the supplied preconditions at
    /// their mutation boundary and use only the reviewed bytes from context.
    fn execute(&self, context: &ActionContext<'_>) -> Result<(), BackendError>;

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
