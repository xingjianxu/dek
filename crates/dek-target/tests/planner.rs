use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
};

use dek_core::{
    ActionKind, ContentDigest, DesiredResource, FileAttributes, FileContent, FileObservation,
    FileResource, FileState, InputKind, InputObservation, NotifyWhen, Observation, Origin,
    Precondition, Resource, ResourceCapability, ResourceId, ResourceKind, ResourceOrigin,
    ResourceSet, ServiceEffect, ServiceNotification, SystemdUnitResource, TargetIdentity,
    UnitDefinition, UnitId, UnitScope,
};
use dek_target::{
    backend::{Backend, BackendError, BackendRegistry, PlannedAction, PlannedNotification},
    planner::{check_compatibility, plan},
};

struct MemoryBackend {
    capabilities: Vec<ResourceCapability>,
    files: RefCell<BTreeMap<String, FileContent>>,
    observations: Cell<usize>,
    running_services: RefCell<BTreeSet<UnitId>>,
    supported_old_versions: BTreeSet<u32>,
}

impl MemoryBackend {
    fn files(version: u32) -> Self {
        Self {
            capabilities: vec![ResourceCapability {
                kind: ResourceKind::File,
                version,
                attributes: ["mode".to_owned()].into_iter().collect(),
            }],
            files: RefCell::default(),
            observations: Cell::default(),
            running_services: RefCell::default(),
            supported_old_versions: BTreeSet::new(),
        }
    }
}

impl Backend for MemoryBackend {
    fn capabilities(&self) -> Vec<ResourceCapability> {
        self.capabilities.clone()
    }

    fn compatible_version(&self, current: &ResourceCapability, planned: u32) -> bool {
        current.version == planned || self.supported_old_versions.contains(&planned)
    }

    fn validate(&self, _: &DesiredResource) -> Result<(), BackendError> {
        Ok(())
    }

    fn observe(&self, resource: &DesiredResource) -> Result<Vec<Precondition>, BackendError> {
        self.observations.set(self.observations.get() + 1);
        if let DesiredResource::SystemdUnit(unit) = resource {
            return Ok(vec![Precondition::UnitRunning {
                unit: unit.unit.clone(),
                expected: self.running_services.borrow().contains(&unit.unit),
            }]);
        }
        let DesiredResource::File(file) = resource else {
            return Ok(Vec::new());
        };
        let expected = self
            .files
            .borrow()
            .get(&file.path)
            .cloned()
            .map_or(FileObservation::Missing, FileObservation::Present);
        Ok(vec![Precondition::File {
            path: file.path.clone(),
            expected,
        }])
    }

    fn plan(
        &self,
        resource: &DesiredResource,
        observed: &[Precondition],
    ) -> Result<Vec<PlannedAction>, BackendError> {
        if let DesiredResource::SystemdUnit(unit) = resource {
            let [Precondition::UnitRunning { expected, .. }] = observed else {
                panic!("unit observation expected");
            };
            if let Some(running) = unit.running
                && running != *expected
            {
                return Ok(vec![PlannedAction {
                    kind: ActionKind::SetUnitRunning {
                        unit: unit.unit.clone(),
                        running,
                    },
                    reason: "service running state differs".into(),
                    preconditions: observed.to_vec(),
                    content_changed: false,
                }]);
            }
            return Ok(Vec::new());
        }
        let DesiredResource::File(file) = resource else {
            return Ok(Vec::new());
        };
        let [Precondition::File { expected, .. }] = observed else {
            panic!("file observation expected");
        };
        let (kind, content_changed) = match (&file.state, expected) {
            (FileState::Present(desired), FileObservation::Present(actual))
                if desired == actual =>
            {
                return Ok(Vec::new());
            }
            (FileState::Present(desired), FileObservation::Present(actual))
                if desired.digest == actual.digest =>
            {
                (
                    ActionKind::SetFileAttributes {
                        path: file.path.clone(),
                        attributes: desired.attributes.clone(),
                    },
                    false,
                )
            }
            (FileState::Present(content), _) => (
                ActionKind::WriteFile {
                    path: file.path.clone(),
                    content: content.clone(),
                },
                true,
            ),
            (FileState::Absent, FileObservation::Missing) => return Ok(Vec::new()),
            (FileState::Absent, _) => (
                ActionKind::RemoveFile {
                    path: file.path.clone(),
                },
                true,
            ),
        };
        Ok(vec![PlannedAction {
            kind,
            reason: "file differs".to_owned(),
            preconditions: observed.to_vec(),
            content_changed,
        }])
    }

    fn plan_notification(
        &self,
        notification: &ServiceNotification,
        service_will_start: bool,
    ) -> Result<Option<PlannedNotification>, BackendError> {
        if !service_will_start && !self.running_services.borrow().contains(&notification.unit) {
            return Ok(None);
        }
        Ok(Some(PlannedNotification {
            reason: "service configuration changed".into(),
            preconditions: vec![Precondition::UnitRunning {
                unit: notification.unit.clone(),
                expected: true,
            }],
        }))
    }

    fn execute(&self, _: &dek_target::backend::ActionContext<'_>) -> Result<(), BackendError> {
        panic!("planning must not call execute")
    }

    fn verify(&self, _: &ActionKind) -> Result<bool, BackendError> {
        panic!("planning must not call verify")
    }
}

fn registry(backend: &dyn Backend) -> BackendRegistry<'_> {
    let mut registry = BackendRegistry::new(
        TargetIdentity {
            id: "test-host".to_owned(),
            os: "test-os".to_owned(),
            distribution: Some("test-distribution".to_owned()),
            architecture: "test-arch".to_owned(),
        },
        1,
        |path| Ok(path.replace('\\', "/").to_ascii_lowercase()),
    );
    registry.register(backend).unwrap();
    registry
}

fn origin(module: &str, line: u32) -> ResourceOrigin {
    ResourceOrigin {
        module: module.to_owned(),
        origin: Origin::Ruby {
            file: format!("profiles/{module}.rb"),
            line: line.try_into().unwrap(),
        },
    }
}

fn content(digest: &str) -> FileContent {
    FileContent {
        digest: ContentDigest(digest.to_owned()),
        attributes: FileAttributes::default(),
    }
}

fn file(path: &str, digest: &str) -> Resource {
    Resource {
        desired: DesiredResource::File(FileResource {
            path: path.to_owned(),
            state: FileState::Present(content(digest)),
        }),
        origins: vec![origin("files", 8)],
        depends_on: Vec::new(),
        notifications: Vec::new(),
    }
}

fn service_id() -> UnitId {
    UnitId {
        scope: UnitScope::System,
        name: "backup.service".into(),
    }
}

fn set(resources: Vec<Resource>) -> ResourceSet {
    ResourceSet { resources }
}

#[test]
fn replans_the_same_declarations_against_live_file_state() {
    let backend = MemoryBackend::files(1);
    let registry = registry(&backend);
    let path = r"C:\managed\Config";
    backend
        .files
        .borrow_mut()
        .insert(path.into(), content("approved"));
    let resources = set(vec![file(path, "approved")]);
    let inputs = vec![InputObservation {
        kind: InputKind::TargetFact,
        id: "architecture".into(),
        observation: Observation::Digest("arch-fingerprint".into()),
        origin: Some(origin("files", 7).origin),
    }];
    let clean = plan(&registry, "repo", &resources, &inputs).unwrap();
    assert!(clean.actions.is_empty());

    backend
        .files
        .borrow_mut()
        .insert(path.into(), content("drifted"));
    let changed = plan(&registry, "repo", &resources, &inputs).unwrap();
    assert_eq!(changed.target.id, "test-host");
    assert_eq!(changed.repository_id, "repo");
    assert_eq!(changed.used_inputs, inputs);
    assert_eq!(changed.resource_digest, clean.resource_digest);
    assert!(changed.resource_digest.0.starts_with("sha256:"));
    assert_ne!(changed.id, clean.id);
    assert_eq!(
        changed,
        plan(&registry, "repo", &resources, &inputs).unwrap()
    );
    let [write] = &changed.actions[..] else {
        panic!("expected one file change");
    };
    assert!(
        matches!(&write.kind, ActionKind::WriteFile { path: raw, content: desired }
        if raw == path && desired == &content("approved"))
    );
    assert_eq!(write.origins, resources.resources[0].origins);
    assert_eq!(
        write.preconditions,
        [Precondition::File {
            path: path.into(),
            expected: FileObservation::Present(content("drifted")),
        }]
    );
    assert_eq!(backend.files.borrow()[path], content("drifted"));
}

#[test]
fn validates_the_entire_set_before_observing_supported_resources() {
    let backend = MemoryBackend::files(1);
    let registry = registry(&backend);
    let service = Resource {
        desired: DesiredResource::SystemdUnit(SystemdUnitResource {
            unit: service_id(),
            definition: UnitDefinition::default(),
            enabled: None,
            running: Some(true),
            activates: None,
        }),
        origins: vec![origin("backup", 12)],
        depends_on: Vec::new(),
        notifications: Vec::new(),
    };
    let error = plan(
        &registry,
        "repo",
        &set(vec![file("/good", "ok"), service]),
        &[],
    )
    .unwrap_err()
    .to_string();
    for part in ["test-host", "SystemdUnit", "profiles/backup.rb:12"] {
        assert!(error.contains(part), "{error}");
    }

    let mut unsupported = file("/unsupported", "ok");
    let DesiredResource::File(FileResource {
        state: FileState::Present(desired),
        ..
    }) = &mut unsupported.desired
    else {
        unreachable!();
    };
    desired.attributes.owner = Some("alice".into());
    let error = plan(
        &registry,
        "repo",
        &set(vec![file("/good", "ok"), unsupported]),
        &[],
    )
    .unwrap_err()
    .to_string();
    for part in ["test-host", "owner", "profiles/files.rb:8"] {
        assert!(error.contains(part), "{error}");
    }
    assert_eq!(backend.observations.get(), 0);
}

#[test]
fn aliases_use_target_path_identity_and_dependencies_cross_no_op_resources() {
    let backend = MemoryBackend::files(1);
    let registry = registry(&backend);
    let mut alias = file("c:/managed/config", "different");
    alias.origins = vec![origin("alias", 2)];
    let collides = set(vec![file(r"C:\Managed\Config", "first"), alias]);
    let error = plan(&registry, "repo", &collides, &[])
        .unwrap_err()
        .to_string();
    for part in [
        "test-host",
        "profiles/files.rb:8",
        "profiles/alias.rb:2",
        "c:/managed/config",
    ] {
        assert!(error.contains(part), "{error}");
    }
    assert_eq!(backend.observations.get(), 0);

    backend
        .files
        .borrow_mut()
        .insert("/middle".into(), content("same"));
    let mut middle = file("/middle", "same");
    middle.depends_on = vec![ResourceId::Path("/Z-FIRST".into())];
    let mut last = file("/a-last", "last");
    last.depends_on = vec![middle.id()];
    let resources = set(vec![last, middle, file("/z-first", "first")]);
    let plan_before = plan(&registry, "repo", &resources, &[]).unwrap();
    let [first, last] = &plan_before.actions[..] else {
        panic!("expected only two changed resources");
    };
    assert!(matches!(&first.kind, ActionKind::WriteFile { path, .. } if path == "/z-first"));
    assert!(matches!(&last.kind, ActionKind::WriteFile { path, .. } if path == "/a-last"));
    assert_eq!(last.depends_on, std::slice::from_ref(&first.id));
    let after_removal = plan(&registry, "repo", &ResourceSet::default(), &[]).unwrap();
    assert!(after_removal.actions.is_empty());
}

#[test]
fn verifies_only_the_capabilities_used_by_the_reviewed_plan() {
    let original = MemoryBackend::files(1);
    let mut resource = file("/profile", "reviewed");
    let DesiredResource::File(FileResource {
        state: FileState::Present(content),
        ..
    }) = &mut resource.desired
    else {
        unreachable!();
    };
    content.attributes.mode = Some("0644".into());
    let reviewed = plan(&registry(&original), "repo", &set(vec![resource]), &[]).unwrap();
    assert_eq!(
        reviewed.used_capabilities[0].attributes,
        ["mode".to_owned()].into_iter().collect()
    );

    let mut upgraded = MemoryBackend::files(1);
    upgraded.capabilities[0].attributes.insert("owner".into());
    upgraded.capabilities.push(ResourceCapability {
        kind: ResourceKind::Package,
        version: 9,
        attributes: BTreeSet::new(),
    });
    check_compatibility(&registry(&upgraded), &reviewed).unwrap();

    upgraded.capabilities[0].version = 2;
    let error = check_compatibility(&registry(&upgraded), &reviewed)
        .unwrap_err()
        .to_string();
    for part in ["File", "test-host", "1", "2"] {
        assert!(error.contains(part), "{error}");
    }
    upgraded.supported_old_versions.insert(1);
    check_compatibility(&registry(&upgraded), &reviewed).unwrap();
    upgraded.capabilities[0].attributes.remove("mode");
    assert!(
        check_compatibility(&registry(&upgraded), &reviewed)
            .unwrap_err()
            .to_string()
            .contains("mode")
    );
}

#[test]
fn change_notifications_follow_writes_and_fold_to_one_service_action() {
    let mut backend = MemoryBackend::files(1);
    backend.capabilities.push(ResourceCapability {
        kind: ResourceKind::SystemdUnit,
        version: 1,
        attributes: ["reload".to_owned(), "restart".to_owned()]
            .into_iter()
            .collect(),
    });
    backend.running_services.borrow_mut().insert(service_id());
    let registry = registry(&backend);
    let mut first = file("/etc/backup/first", "first");
    let mut second = file("/etc/backup/second", "second");
    first.notifications.push(ServiceNotification {
        unit: service_id(),
        effect: ServiceEffect::Reload,
        when: NotifyWhen::ContentChanged,
    });
    second.notifications.push(ServiceNotification {
        unit: service_id(),
        effect: ServiceEffect::Restart,
        when: NotifyWhen::Changed,
    });
    second.origins = vec![origin("backup", 15)];

    let review = plan(&registry, "repo", &set(vec![second, first]), &[]).unwrap();
    let systemd = review
        .used_capabilities
        .iter()
        .find(|capability| capability.kind == ResourceKind::SystemdUnit)
        .unwrap();
    assert_eq!(
        systemd.attributes,
        ["reload".to_owned(), "restart".to_owned()]
            .into_iter()
            .collect()
    );
    let [first, second, notify] = &review.actions[..] else {
        panic!("two writes and one notification expected")
    };
    assert_eq!(notify.depends_on, [first.id.clone(), second.id.clone()]);
    assert_eq!(notify.origins, [origin("files", 8), origin("backup", 15)]);
    assert_eq!(
        notify.kind,
        ActionKind::NotifyService {
            unit: service_id(),
            effect: ServiceEffect::Restart,
            sources: vec![
                dek_core::NotificationSource {
                    action: first.id.clone(),
                    effect: ServiceEffect::Reload,
                    when: NotifyWhen::ContentChanged,
                },
                dek_core::NotificationSource {
                    action: second.id.clone(),
                    effect: ServiceEffect::Restart,
                    when: NotifyWhen::Changed,
                },
            ],
        }
    );
    assert_eq!(
        notify.preconditions,
        [Precondition::UnitRunning {
            unit: service_id(),
            expected: true
        }]
    );
}

#[test]
fn invalid_resource_graph_is_rejected_before_observation() {
    let backend = MemoryBackend::files(1);
    let registry = registry(&backend);
    let mut first = file("/first", "first");
    first.depends_on = vec![ResourceId::Path("/missing".into())];
    let error = plan(&registry, "repo", &set(vec![first.clone()]), &[])
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("/missing") && error.contains("profiles/files.rb:8"),
        "{error}"
    );

    let mut second = file("/second", "second");
    first.depends_on = vec![second.id()];
    second.depends_on = vec![first.id()];
    let error = plan(&registry, "repo", &set(vec![first, second]), &[])
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("cycle") && error.contains("/first") && error.contains("/second"),
        "{error}"
    );
    assert_eq!(backend.observations.get(), 0);
}

#[test]
fn validates_notification_effects_before_observation() {
    let mut backend = MemoryBackend::files(1);
    backend.capabilities.push(ResourceCapability {
        kind: ResourceKind::SystemdUnit,
        version: 1,
        attributes: BTreeSet::new(),
    });
    let registry = registry(&backend);
    let mut configuration = file("/etc/backup.conf", "approved");
    configuration.notifications.push(ServiceNotification {
        unit: service_id(),
        effect: ServiceEffect::Reload,
        when: NotifyWhen::ContentChanged,
    });
    let error = plan(&registry, "repo", &set(vec![configuration]), &[])
        .unwrap_err()
        .to_string();
    for part in ["SystemdUnit", "reload", "profiles/files.rb:8", "test-host"] {
        assert!(error.contains(part), "{error}");
    }
    assert_eq!(backend.observations.get(), 0);
}

#[test]
fn attributes_do_not_trigger_content_notifications_and_startup_uses_new_content() {
    let mut backend = MemoryBackend::files(1);
    backend.capabilities.push(ResourceCapability {
        kind: ResourceKind::SystemdUnit,
        version: 1,
        attributes: ["running".to_owned(), "reload".to_owned()]
            .into_iter()
            .collect(),
    });
    let registry = registry(&backend);
    let path = "/etc/backup.conf";
    let mut configuration = file(path, "approved");
    let DesiredResource::File(FileResource {
        state: FileState::Present(content_with_mode),
        ..
    }) = &mut configuration.desired
    else {
        unreachable!();
    };
    content_with_mode.attributes.mode = Some("0644".into());
    configuration.notifications.push(ServiceNotification {
        unit: service_id(),
        effect: ServiceEffect::Reload,
        when: NotifyWhen::ContentChanged,
    });

    backend
        .files
        .borrow_mut()
        .insert(path.into(), content("approved"));
    backend.running_services.borrow_mut().insert(service_id());
    let reviewed = plan(&registry, "repo", &set(vec![configuration.clone()]), &[]).unwrap();
    assert!(
        matches!(&reviewed.actions[..], [action] if matches!(action.kind, ActionKind::SetFileAttributes { .. }))
    );

    backend.files.borrow_mut().clear();
    backend.running_services.borrow_mut().clear();
    let service = Resource {
        desired: DesiredResource::SystemdUnit(SystemdUnitResource {
            unit: service_id(),
            definition: UnitDefinition::default(),
            enabled: None,
            running: Some(true),
            activates: None,
        }),
        origins: vec![origin("backup", 12)],
        depends_on: Vec::new(),
        notifications: Vec::new(),
    };
    let reviewed = plan(&registry, "repo", &set(vec![service, configuration]), &[]).unwrap();
    let [write, start] = &reviewed.actions[..] else {
        panic!("a write and a start expected")
    };
    assert!(matches!(write.kind, ActionKind::WriteFile { .. }));
    assert!(matches!(
        start.kind,
        ActionKind::SetUnitRunning { running: true, .. }
    ));
    assert_eq!(start.depends_on, std::slice::from_ref(&write.id));
}

#[test]
fn registration_reports_identity_and_does_not_partially_add_conflicting_backend() {
    let file_backend = MemoryBackend::files(1);
    let mut conflicting = MemoryBackend::files(2);
    conflicting.capabilities.insert(
        0,
        ResourceCapability {
            kind: ResourceKind::Group,
            version: 3,
            attributes: BTreeSet::new(),
        },
    );
    let mut registry = registry(&file_backend);
    let error = registry.register(&conflicting).unwrap_err();
    assert_eq!(error.kind, ResourceKind::File);
    let offered = registry.capabilities();
    assert_eq!(offered.protocol_version, 1);
    assert_eq!(offered.target.id, "test-host");
    assert_eq!(offered.resources, file_backend.capabilities);
}

#[test]
fn plans_notifications_against_ordered_service_state_changes() {
    let mut backend = MemoryBackend::files(1);
    backend.capabilities.push(ResourceCapability {
        kind: ResourceKind::SystemdUnit,
        version: 1,
        attributes: ["running".to_owned(), "reload".to_owned()]
            .into_iter()
            .collect(),
    });
    let registry = registry(&backend);
    let mut configuration = file("/etc/backup.conf", "approved");
    configuration.notifications.push(ServiceNotification {
        unit: service_id(),
        effect: ServiceEffect::Reload,
        when: NotifyWhen::ContentChanged,
    });
    let service = |running| Resource {
        desired: DesiredResource::SystemdUnit(SystemdUnitResource {
            unit: service_id(),
            definition: UnitDefinition::default(),
            enabled: None,
            running: Some(running),
            activates: None,
        }),
        origins: vec![origin("backup", 12)],
        depends_on: Vec::new(),
        notifications: Vec::new(),
    };

    backend.running_services.borrow_mut().insert(service_id());
    let stopped = plan(
        &registry,
        "repo",
        &set(vec![configuration.clone(), service(false)]),
        &[],
    )
    .unwrap();
    let [_write, stop] = &stopped.actions[..] else {
        panic!("a write and a stop expected")
    };
    assert!(matches!(
        stop.kind,
        ActionKind::SetUnitRunning { running: false, .. }
    ));

    backend.running_services.borrow_mut().clear();
    let started = service(true);
    configuration.depends_on.push(started.id());
    let reviewed = plan(&registry, "repo", &set(vec![configuration, started]), &[]).unwrap();
    let [start, write, notify] = &reviewed.actions[..] else {
        panic!("a start, write, and notify expected")
    };
    assert!(matches!(
        start.kind,
        ActionKind::SetUnitRunning { running: true, .. }
    ));
    assert_eq!(write.depends_on, std::slice::from_ref(&start.id));
    assert!(notify.depends_on.contains(&write.id));
    assert!(matches!(notify.kind, ActionKind::NotifyService { .. }));
}
