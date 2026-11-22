//! Read-only planning from a complete resource set and current target state.

mod graph;
mod systemd;

use std::collections::{BTreeMap, BTreeSet};

use dek_core::{
    Action, ActionKind, ContentDigest, InputObservation, NotificationSource, NotifyWhen, Plan,
    PlanVersion, Resource, ResourceId, ResourceKind, ResourceOrigin, ResourceSet, ServiceEffect,
    ServiceNotification, UnitId,
};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::backend::{BackendRegistry, PlannedPackages, PlanningContext};
pub use dek_core::PlanMode;
use graph::{prepare, topological_order};

#[derive(Debug, Error)]
pub enum PlanError {
    #[error("target {target}: {resource:?} at {locations}: {message}")]
    Resource {
        target: String,
        resource: ResourceId,
        locations: String,
        message: String,
    },
    #[error("target {target}: resource dependency cycle blocks {resources}")]
    Cycle { target: String, resources: String },
    #[error("reviewed plan targets {planned:?}; current target is {current:?}")]
    TargetMismatch { planned: String, current: String },
    #[error("target {target}: incompatible {kind:?} capability: {message}")]
    Capability {
        target: String,
        kind: ResourceKind,
        message: String,
    },
    #[error("目标 {target} 无法预览软件包事务（{locations}）：{message}")]
    Packages {
        target: String,
        locations: String,
        message: String,
    },
    #[error("cannot encode the reviewed plan or resource digest: {0}")]
    Encode(#[from] serde_json::Error),
}

pub(super) fn resource_error(
    registry: &BackendRegistry<'_>,
    resource: &Resource,
    message: impl Into<String>,
) -> PlanError {
    PlanError::Resource {
        target: registry.target().id.clone(),
        resource: resource.id(),
        locations: locations(&resource.origins),
        message: message.into(),
    }
}

pub(super) fn locations(origins: &[ResourceOrigin]) -> String {
    if origins.is_empty() {
        "unknown source".into()
    } else {
        origins
            .iter()
            .map(|source| format!("{} ({})", source.origin, source.module))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Build a plan for the registered target without calling any execution hooks.
/// `used_inputs` contains the observations used by management-side evaluation.
pub fn plan(
    registry: &BackendRegistry<'_>,
    repository_id: &str,
    resources: &ResourceSet,
    used_inputs: &[InputObservation],
) -> Result<Plan, PlanError> {
    plan_with_host(
        registry,
        repository_id,
        None,
        resources,
        used_inputs,
        &BTreeMap::new(),
        PlanMode::Normal,
    )
}

/// Build a reviewed plan whose ID also binds the exact repository host name.
pub fn plan_for_host(
    registry: &BackendRegistry<'_>,
    repository_id: &str,
    logical_host: &str,
    resources: &ResourceSet,
    used_inputs: &[InputObservation],
) -> Result<Plan, PlanError> {
    plan_for_host_with_contents(
        registry,
        repository_id,
        logical_host,
        resources,
        used_inputs,
        &BTreeMap::new(),
    )
}

/// Build a host plan with the immutable blobs referenced by its desired resources.
pub fn plan_for_host_with_contents(
    registry: &BackendRegistry<'_>,
    repository_id: &str,
    logical_host: &str,
    resources: &ResourceSet,
    used_inputs: &[InputObservation],
    contents: &BTreeMap<ContentDigest, Vec<u8>>,
) -> Result<Plan, PlanError> {
    plan_for_host_with_contents_mode(
        registry,
        repository_id,
        logical_host,
        resources,
        used_inputs,
        contents,
        PlanMode::Normal,
    )
}

/// Build an ordinary or full-upgrade plan using the selected target backend.
pub fn plan_for_host_with_contents_mode(
    registry: &BackendRegistry<'_>,
    repository_id: &str,
    logical_host: &str,
    resources: &ResourceSet,
    used_inputs: &[InputObservation],
    contents: &BTreeMap<ContentDigest, Vec<u8>>,
    mode: PlanMode,
) -> Result<Plan, PlanError> {
    plan_with_host(
        registry,
        repository_id,
        Some(logical_host),
        resources,
        used_inputs,
        contents,
        mode,
    )
}

fn plan_with_host(
    registry: &BackendRegistry<'_>,
    repository_id: &str,
    logical_host: Option<&str>,
    resources: &ResourceSet,
    used_inputs: &[InputObservation],
    contents: &BTreeMap<ContentDigest, Vec<u8>>,
    mode: PlanMode,
) -> Result<Plan, PlanError> {
    let (prepared, mut used_capabilities) = prepare(registry, resources)?;
    let order = topological_order(registry, &prepared)?;
    let mut actions = Actions::default();
    let mut packages = plan_packages(registry, resources, mode)?;
    let transaction = packages.transaction.take().map(|draft| {
        let origins = packages
            .covered
            .iter()
            .flat_map(|id| prepared[id].resource.origins.clone())
            .collect();
        if !used_capabilities
            .iter()
            .any(|capability| capability.kind == ResourceKind::Package)
        {
            let (capability, _) = registry
                .backend(ResourceKind::Package)
                .expect("package transaction backend");
            used_capabilities.push(capability.clone());
            used_capabilities.sort_by_key(|capability| capability.kind);
        }
        actions.push(
            draft.kind,
            draft.reason,
            origins,
            &BTreeSet::new(),
            draft.preconditions,
        );
        0
    });
    let context = PlanningContext {
        resources,
        contents,
    };

    for id in order {
        let prepared_resource = &prepared[&id];
        let resource = prepared_resource.resource;
        let (_, backend) = registry
            .backend(resource.desired.kind())
            .expect("validated backend");
        let mut predecessors = prepared_resource
            .dependencies
            .iter()
            .flat_map(|dependency| actions.ends[dependency].iter().copied())
            .collect::<BTreeSet<_>>();
        let covered = packages.covered.contains(&id);
        if covered && predecessors.iter().any(|index| Some(*index) != transaction) {
            return Err(resource_error(
                registry,
                resource,
                "软件包事务依赖尚未应用的配置，请先应用前置配置",
            ));
        }
        predecessors.extend(transaction);
        let drafts = match packages.actions.remove(&id) {
            Some(drafts) => drafts,
            None if covered => Vec::new(),
            None => backend
                .plan_with_contents(&resource.desired, &context)
                .map_err(|err| resource_error(registry, resource, err.0))?,
        };

        let changed = !drafts.is_empty();
        let content_changed = drafts.iter().any(|action| action.content_changed);
        for draft in drafts {
            let next = actions.records.len();
            if let ActionKind::SetUnitRunning { unit, running } = &draft.kind {
                actions
                    .running_changes
                    .insert(unit.clone(), (*running, next));
            }
            actions.push(
                draft.kind,
                draft.reason,
                resource.origins.clone(),
                &predecessors,
                draft.preconditions,
            );
            predecessors = BTreeSet::from([next]);
        }
        if changed {
            let last_action = actions.records.len() - 1;
            for notification in &resource.notifications {
                if notification.when == NotifyWhen::Changed || content_changed {
                    actions.record_notification(notification, last_action, &resource.origins);
                }
            }
        }

        actions.ends.insert(id, predecessors);
    }

    let actions = actions.finish(registry)?;
    let mut reviewed = Plan {
        format_version: PlanVersion::V1,
        id: String::new(),
        repository_id: repository_id.into(),
        target: registry.target().clone(),
        resource_digest: resource_digest(resources)?,
        used_capabilities,
        used_inputs: used_inputs.to_vec(),
        actions,
    };
    reviewed.id = plan_id(&reviewed, logical_host)?;
    Ok(reviewed)
}

fn plan_packages(
    registry: &BackendRegistry<'_>,
    resources: &ResourceSet,
    mode: PlanMode,
) -> Result<PlannedPackages, PlanError> {
    let error = |message| PlanError::Packages {
        target: registry.target().id.clone(),
        locations: locations(
            &resources
                .resources
                .iter()
                .filter(|resource| resource.desired.kind() == ResourceKind::Package)
                .flat_map(|resource| resource.origins.clone())
                .collect::<Vec<_>>(),
        ),
        message,
    };
    let planned = match registry.backend(ResourceKind::Package) {
        Some((_, backend)) => backend
            .plan_packages(resources, mode)
            .map_err(|err| error(err.0))?,
        None => None,
    };
    if planned.is_none() && mode == PlanMode::Upgrade {
        return Err(error("目标未提供完整仓库升级预览能力".into()));
    }
    Ok(planned.unwrap_or_default())
}

pub(crate) fn resource_digest(resources: &ResourceSet) -> Result<ContentDigest, serde_json::Error> {
    let digest = Sha256::digest(serde_json::to_vec(resources)?);
    Ok(ContentDigest(format!("sha256:{digest:x}")))
}

pub(crate) fn plan_id(
    plan: &Plan,
    logical_host: Option<&str>,
) -> Result<String, serde_json::Error> {
    let blank;
    let plan = if plan.id.is_empty() {
        plan
    } else {
        blank = Plan {
            id: String::new(),
            ..plan.clone()
        };
        &blank
    };
    let data = match logical_host {
        Some(host) => serde_json::to_vec(&("dek-reviewed-plan-v1", host, plan))?,
        None => serde_json::to_vec(plan)?,
    };
    Ok(format!("{:x}", Sha256::digest(data)))
}

/// Recheck the target binding and exactly the capabilities recorded by the plan.
pub fn check_compatibility(
    registry: &BackendRegistry<'_>,
    reviewed: &Plan,
) -> Result<(), PlanError> {
    let current = &registry.target().id;
    if reviewed.target.id != *current {
        return Err(PlanError::TargetMismatch {
            planned: reviewed.target.id.clone(),
            current: current.clone(),
        });
    }

    for planned in &reviewed.used_capabilities {
        let incompatible = |message| PlanError::Capability {
            target: current.clone(),
            kind: planned.kind,
            message,
        };
        let (capability, backend) = registry
            .backend(planned.kind)
            .ok_or_else(|| incompatible("resource kind is unavailable".into()))?;
        if !backend.compatible_version(capability, planned.version) {
            return Err(incompatible(format!(
                "reviewed version {}, current version {}",
                planned.version, capability.version
            )));
        }
        if let Some(attribute) = planned.attributes.difference(&capability.attributes).next() {
            return Err(incompatible(format!(
                "reviewed attribute {attribute:?} is unavailable"
            )));
        }
    }
    Ok(())
}

#[derive(Default)]
struct Actions {
    records: Vec<Action>,
    ends: BTreeMap<ResourceId, BTreeSet<usize>>,
    running_changes: BTreeMap<UnitId, (bool, usize)>,
    notifications: BTreeMap<UnitId, PendingNotification>,
}

struct PendingNotification {
    notification: ServiceNotification,
    origins: Vec<ResourceOrigin>,
    sources: BTreeMap<(usize, NotifyWhen), ServiceEffect>,
}

impl Actions {
    fn push(
        &mut self,
        kind: ActionKind,
        reason: String,
        origins: Vec<ResourceOrigin>,
        predecessors: &BTreeSet<usize>,
        preconditions: Vec<dek_core::Precondition>,
    ) {
        self.records.push(Action {
            id: action_id(self.records.len()),
            kind,
            reason,
            origins,
            depends_on: predecessors.iter().copied().map(action_id).collect(),
            preconditions,
        });
    }

    fn record_notification(
        &mut self,
        notification: &ServiceNotification,
        predecessor: usize,
        origins: &[ResourceOrigin],
    ) {
        let pending = self
            .notifications
            .entry(notification.unit.clone())
            .or_insert_with(|| PendingNotification {
                notification: notification.clone(),
                origins: Vec::new(),
                sources: BTreeMap::new(),
            });
        if notification.effect == ServiceEffect::Restart {
            pending.notification.effect = ServiceEffect::Restart;
        }
        if notification.when == NotifyWhen::Changed {
            pending.notification.when = NotifyWhen::Changed;
        }
        let effect = pending
            .sources
            .entry((predecessor, notification.when))
            .or_insert(notification.effect);
        if notification.effect == ServiceEffect::Restart {
            *effect = ServiceEffect::Restart;
        }
        for origin in origins {
            if !pending.origins.contains(origin) {
                pending.origins.push(origin.clone());
            }
        }
    }

    fn finish(mut self, registry: &BackendRegistry<'_>) -> Result<Vec<Action>, PlanError> {
        for (unit, pending) in std::mem::take(&mut self.notifications) {
            let running_change = self.running_changes.get(&unit).copied();
            if matches!(running_change, Some((false, _))) {
                continue;
            }
            if let Some((true, start)) = running_change
                && pending.sources.keys().all(|&(index, _)| index < start)
            {
                let depends_on = &mut self.records[start].depends_on;
                for id in pending.sources.keys().map(|&(index, _)| action_id(index)) {
                    if !depends_on.contains(&id) {
                        depends_on.push(id);
                    }
                }
                continue;
            }

            let (_, backend) = registry
                .backend(ResourceKind::SystemdUnit)
                .expect("validated notification backend");
            let planned = backend
                .plan_notification(&pending.notification, running_change.is_some())
                .map_err(|err| PlanError::Resource {
                    target: registry.target().id.clone(),
                    resource: ResourceId::SystemdUnit(unit.clone()),
                    locations: locations(&pending.origins),
                    message: format!("plan service notification: {err}"),
                })?;
            if let Some(planned) = planned {
                let mut predecessors: BTreeSet<_> =
                    pending.sources.keys().map(|&(index, _)| index).collect();
                if let Some(ends) = self.ends.get(&ResourceId::SystemdUnit(unit.clone())) {
                    predecessors.extend(ends);
                }
                let sources = pending
                    .sources
                    .into_iter()
                    .map(|((index, when), effect)| NotificationSource {
                        action: action_id(index),
                        effect,
                        when,
                    })
                    .collect();
                self.push(
                    ActionKind::NotifyService {
                        unit,
                        effect: pending.notification.effect,
                        sources,
                    },
                    planned.reason,
                    pending.origins,
                    &predecessors,
                    planned.preconditions,
                );
            }
        }
        systemd::coalesce_reload(self.records, registry.target())
    }
}

fn action_id(index: usize) -> String {
    format!("action-{}", index + 1)
}
