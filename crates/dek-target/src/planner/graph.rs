use std::collections::{BTreeMap, BTreeSet};

use dek_core::{
    DesiredResource, FileAttributes, FileState, PackageSource, Resource, ResourceCapability,
    ResourceId, ResourceKind, ResourceSet, ServiceEffect, UnitScope,
};

use super::{PlanError, locations, resource_error};
use crate::backend::{Backend, BackendRegistry};

pub(super) struct PreparedResource<'a> {
    pub(super) resource: &'a Resource,
    pub(super) dependencies: BTreeSet<ResourceId>,
}

type ResourceGraph<'a> = BTreeMap<ResourceId, PreparedResource<'a>>;

pub(super) fn prepare<'a>(
    registry: &BackendRegistry<'_>,
    resources: &'a ResourceSet,
) -> Result<(ResourceGraph<'a>, Vec<ResourceCapability>), PlanError> {
    let mut graph = ResourceGraph::new();
    let mut used = BTreeMap::new();
    let managed: Vec<_> = resources.resources.iter().map(Resource::id).collect();

    for (resource, id) in resources.resources.iter().zip(&managed) {
        let id = normalize_id(registry, resource, id.clone())?;
        if let Some(previous) = graph.get(&id) {
            return Err(resource_error(
                registry,
                resource,
                format!(
                    "target resource alias {id:?} already declared as {:?} at {}",
                    previous.resource.id(),
                    locations(&previous.resource.origins),
                ),
            ));
        }

        let attributes = required_attributes(&resource.desired);
        let (capability, backend) =
            require_backend(registry, resource, resource.desired.kind(), &attributes)?;
        backend
            .validate(&resource.desired)
            .and_then(|()| backend.validate_managed_paths(&resource.desired, &managed))
            .map_err(|err| resource_error(registry, resource, format!("validate: {err}")))?;
        record_capability(&mut used, capability, attributes);

        for notification in &resource.notifications {
            let mut attributes = unit_scope_attributes(&notification.unit.scope);
            attributes.push(match notification.effect {
                ServiceEffect::Reload => "reload",
                ServiceEffect::Restart => "restart",
            });
            let (capability, backend) =
                require_backend(registry, resource, ResourceKind::SystemdUnit, &attributes)?;
            backend.validate_notification(notification).map_err(|err| {
                resource_error(
                    registry,
                    resource,
                    format!(
                        "validate service notification {:?}: {err}",
                        notification.unit
                    ),
                )
            })?;
            record_capability(&mut used, capability, attributes);
        }

        let dependencies = resource
            .depends_on
            .iter()
            .cloned()
            .map(|dependency| normalize_id(registry, resource, dependency))
            .collect::<Result<_, _>>()?;
        graph.insert(
            id,
            PreparedResource {
                resource,
                dependencies,
            },
        );
    }

    Ok((graph, used.into_values().collect()))
}

fn normalize_id(
    registry: &BackendRegistry<'_>,
    resource: &Resource,
    id: ResourceId,
) -> Result<ResourceId, PlanError> {
    match id {
        ResourceId::Path(path) => registry
            .normalize_path(&path)
            .map(ResourceId::Path)
            .map_err(|err| {
                resource_error(
                    registry,
                    resource,
                    format!("normalize target path {path:?}: {err}"),
                )
            }),
        other => Ok(other),
    }
}

fn require_backend<'a>(
    registry: &'a BackendRegistry<'_>,
    resource: &Resource,
    kind: ResourceKind,
    attributes: &[&str],
) -> Result<(&'a ResourceCapability, &'a dyn Backend), PlanError> {
    let (capability, backend) = registry.backend(kind).ok_or_else(|| {
        resource_error(registry, resource, format!("missing {kind:?} capability"))
    })?;
    if let Some(&attribute) = attributes
        .iter()
        .find(|&&attribute| !capability.attributes.contains(attribute))
    {
        return Err(resource_error(
            registry,
            resource,
            format!("missing {kind:?} capability attribute {attribute:?}"),
        ));
    }
    Ok((capability, backend))
}

fn record_capability(
    used: &mut BTreeMap<ResourceKind, ResourceCapability>,
    available: &ResourceCapability,
    attributes: Vec<&str>,
) {
    let capability = used
        .entry(available.kind)
        .or_insert_with(|| ResourceCapability {
            kind: available.kind,
            version: available.version,
            attributes: BTreeSet::new(),
        });
    capability
        .attributes
        .extend(attributes.into_iter().map(str::to_owned));
}

fn file_attributes(attributes: &FileAttributes) -> Vec<&'static str> {
    present([
        ("owner", attributes.owner.is_some()),
        ("group", attributes.group.is_some()),
        ("mode", attributes.mode.is_some()),
    ])
}

fn unit_scope_attributes(scope: &UnitScope) -> Vec<&'static str> {
    match scope {
        UnitScope::System => Vec::new(),
        UnitScope::UserGlobal => vec!["userGlobal"],
        UnitScope::User { .. } => vec!["user"],
    }
}

fn required_attributes(resource: &DesiredResource) -> Vec<&'static str> {
    match resource {
        DesiredResource::Package(package) => {
            present([("aur", package.package.source == PackageSource::Aur)])
        }
        DesiredResource::User(user) => present([
            ("kind", user.kind.is_some()),
            ("uid", user.uid.is_some()),
            ("group", user.group.is_some()),
            ("extraGroups", user.extra_groups.is_some()),
            ("home", user.home.is_some()),
            ("shell", user.shell.is_some()),
            ("description", user.description.is_some()),
        ]),
        DesiredResource::Group(group) => present([("gid", group.gid.is_some())]),
        DesiredResource::Directory(directory) => file_attributes(&directory.attributes),
        DesiredResource::File(file) => match &file.state {
            FileState::Present(content) => file_attributes(&content.attributes),
            FileState::Absent => Vec::new(),
        },
        DesiredResource::SystemdUnit(unit) => {
            let mut attributes = unit_scope_attributes(&unit.unit.scope);
            attributes.extend(present([
                ("content", unit.definition.content.is_some()),
                ("dropIns", !unit.definition.drop_ins.is_empty()),
                ("enabled", unit.enabled.is_some()),
                ("running", unit.running.is_some()),
                ("activates", unit.activates.is_some()),
            ]));
            attributes
        }
        DesiredResource::AuthorizedKeys(keys) => {
            present([("keyFiles", !keys.key_files.is_empty())])
        }
        DesiredResource::AgeSecret(secret) => file_attributes(&secret.attributes),
    }
}

fn present<const N: usize>(attributes: [(&'static str, bool); N]) -> Vec<&'static str> {
    attributes
        .into_iter()
        .filter_map(|(name, present)| present.then_some(name))
        .collect()
}

pub(super) fn topological_order(
    registry: &BackendRegistry<'_>,
    graph: &ResourceGraph<'_>,
) -> Result<Vec<ResourceId>, PlanError> {
    let mut dependents: BTreeMap<&ResourceId, Vec<&ResourceId>> = BTreeMap::new();
    let mut remaining = BTreeMap::new();
    let mut ready = BTreeSet::new();
    for (id, node) in graph {
        for dependency in &node.dependencies {
            if !graph.contains_key(dependency) {
                return Err(resource_error(
                    registry,
                    node.resource,
                    format!("unknown dependency {dependency:?}"),
                ));
            }
            dependents.entry(dependency).or_default().push(id);
        }
        if node.dependencies.is_empty() {
            ready.insert(id);
        } else {
            remaining.insert(id, node.dependencies.len());
        }
    }

    let mut order = Vec::with_capacity(graph.len());
    while let Some(id) = ready.pop_first() {
        order.push(id.clone());
        for dependent in dependents.remove(id).into_iter().flatten() {
            let count = remaining.get_mut(dependent).expect("unprocessed dependent");
            *count -= 1;
            if *count == 0 {
                remaining.remove(dependent);
                ready.insert(dependent);
            }
        }
    }
    if !remaining.is_empty() {
        let resources = remaining
            .keys()
            .map(|&id| format!("{id:?} at {}", locations(&graph[id].resource.origins)))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(PlanError::Cycle {
            target: registry.target().id.clone(),
            resources,
        });
    }
    Ok(order)
}
