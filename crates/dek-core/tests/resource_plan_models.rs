use std::collections::BTreeSet;

use dek_core::{
    ActionKind, DesiredResource, FileObservation, FileState, NotifyWhen, PackageSource, Plan,
    PlanVersion, Precondition, Presence, ResourceId, ResourceKind, ResourceSet, ServiceEffect,
    UnitScope,
};
use serde_json::{Value, json};

const RESOURCES: &str = include_str!("fixtures/resource-set.json");
const PLAN: &str = include_str!("fixtures/plan-v1.json");
const CONTENT: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";
const OLD_CONTENT: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";

#[test]
fn scoped_units_round_trip_with_independent_enabled_and_running() {
    let set: ResourceSet = serde_json::from_str(RESOURCES).unwrap();
    assert_eq!(
        serde_json::to_value(&set).unwrap(),
        serde_json::from_str::<Value>(RESOURCES).unwrap()
    );
    let encoded = rmp_serde::to_vec(&set).unwrap();
    let decoded: ResourceSet = rmp_serde::from_slice(&encoded).unwrap();
    assert_eq!(decoded, set);

    let expected = [
        (UnitScope::System, Some(true), None),
        (
            UnitScope::User {
                user: "alice".into(),
            },
            None,
            Some(false),
        ),
        (
            UnitScope::User { user: "bob".into() },
            Some(false),
            Some(true),
        ),
    ];
    let mut ids = BTreeSet::new();
    for (resource, (scope, enabled, running)) in decoded.resources[2..].iter().zip(expected) {
        let DesiredResource::SystemdUnit(desired) = &resource.desired else {
            panic!("expected a systemd unit");
        };
        assert_eq!(desired.unit.scope, scope);
        assert_eq!(desired.unit.name, "backup.timer");
        assert_eq!(desired.enabled, enabled);
        assert_eq!(desired.running, running);
        assert!(ids.insert(resource.id()));
    }
    assert_eq!(ids.len(), 3);
    assert_eq!(
        set.resources[3].depends_on,
        [ResourceId::User("alice".into())]
    );
}

#[test]
fn resource_records_keep_raw_paths_provenance_dependencies_and_triggers() {
    let set: ResourceSet = serde_json::from_str(RESOURCES).unwrap();
    let package = &set.resources[0];
    let file = &set.resources[1];

    assert_eq!(
        file.id(),
        ResourceId::Path("/etc/backup/../backup.conf".into())
    );
    assert_eq!(file.depends_on, [package.id()]);
    assert_eq!(file.origins[0].module, "backup");
    assert_eq!(
        file.origins[0].origin.to_string(),
        "hosts/server/default.rb:12"
    );
    assert_eq!(file.notifications[0].unit.name, "backup.service");
    assert_eq!(file.notifications[0].effect, ServiceEffect::Restart);
    assert_eq!(file.notifications[0].when, NotifyWhen::ContentChanged);
    assert_eq!(
        set.content_digests()
            .iter()
            .map(|digest| digest.0.as_str())
            .collect::<Vec<_>>(),
        [CONTENT]
    );
}

#[test]
fn omitted_resources_differ_from_explicit_absent_and_package_sources_differ() {
    let empty = ResourceSet::default();
    let set: ResourceSet = serde_json::from_value(json!({
        "resources": [
            { "desired": { "file": { "path": "C:\\managed\\old.conf", "state": "absent" } } },
            { "desired": { "package": { "package": { "source": "official", "name": "backup" }, "state": "present" } } },
            { "desired": { "package": { "package": { "source": "aur", "name": "backup" }, "state": "absent" } } }
        ]
    }))
    .unwrap();
    assert_ne!(set, empty);
    assert!(set.content_digests().is_empty());
    assert_eq!(
        set.resources[0].id(),
        ResourceId::Path(r"C:\managed\old.conf".into())
    );
    assert!(matches!(
        &set.resources[0].desired,
        DesiredResource::File(file) if file.state == FileState::Absent
    ));
    assert!(matches!(
        &set.resources[2].desired,
        DesiredResource::Package(package) if package.state == Presence::Absent && package.package.source == PackageSource::Aur
    ));
    assert_ne!(set.resources[1].id(), set.resources[2].id());
}

#[test]
fn systemd_fields_can_be_omitted_in_json() {
    let set: ResourceSet = serde_json::from_value(json!({
        "resources": [
            { "desired": { "systemdUnit": { "unit": { "scope": "userGlobal", "name": "backup.timer" }, "enabled": false } } },
            { "desired": { "systemdUnit": { "unit": { "scope": "system", "name": "backup.timer" }, "running": true } } }
        ]
    }))
    .unwrap();

    let DesiredResource::SystemdUnit(first) = &set.resources[0].desired else {
        panic!("expected user-global timer");
    };
    assert_eq!(first.unit.scope, UnitScope::UserGlobal);
    assert_eq!((first.enabled, first.running), (Some(false), None));
    let DesiredResource::SystemdUnit(second) = &set.resources[1].desired else {
        panic!("expected system timer");
    };
    assert_eq!((second.enabled, second.running), (None, Some(true)));
}

#[test]
fn plan_round_trips_with_order_content_target_provenance_and_preconditions() {
    let plan: Plan = serde_json::from_str(PLAN).unwrap();
    assert_eq!(
        serde_json::to_value(&plan).unwrap(),
        serde_json::from_str::<Value>(PLAN).unwrap()
    );
    let encoded = rmp_serde::to_vec(&plan).unwrap();
    let decoded: Plan = rmp_serde::from_slice(&encoded).unwrap();
    assert_eq!(decoded, plan);
    assert_eq!(decoded.format_version, PlanVersion::V1);
    assert_eq!(decoded.repository_id, "repo-backup");
    assert_eq!(decoded.target.id, "target-server");
    assert_eq!(decoded.used_capabilities[0].kind, ResourceKind::File);
    assert_eq!(decoded.used_inputs[0].id, "architecture");

    let [write, restart] = &decoded.actions[..] else {
        panic!("expected two actions");
    };
    assert_eq!(write.id, "write-config");
    assert_eq!(restart.id, "restart-backup");
    assert_eq!(restart.depends_on, [write.id.as_str()]);
    assert!(
        matches!(&write.kind, ActionKind::WriteFile { content, .. } if content.digest.0 == CONTENT)
    );
    assert!(matches!(
        &write.preconditions[0],
        Precondition::File { expected: FileObservation::Present(content), .. } if content.digest.0 == OLD_CONTENT
    ));
    assert_eq!(
        write.origins[0].origin.to_string(),
        "hosts/server/default.rb:12"
    );
    assert_eq!(restart.origins[0].origin.file(), "profiles/backup.json");
    assert_eq!(
        decoded
            .content_digests()
            .iter()
            .map(|digest| digest.0.as_str())
            .collect::<Vec<_>>(),
        [CONTENT]
    );
}

#[test]
fn rejects_unknown_actions_and_unsupported_plan_versions() {
    for (original, replacement, expected) in [
        ("notifyService", "unknownAction", "unknownAction"),
        (
            "\"formatVersion\": 1",
            "\"formatVersion\": 2",
            "unsupported plan format version 2",
        ),
    ] {
        let bad_plan = PLAN.replacen(original, replacement, 1);
        let error = serde_json::from_str::<Plan>(&bad_plan).unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
        assert!(error.line() > 0);
        assert!(error.column() > 0);

        let wire: Value = serde_json::from_str(&bad_plan).unwrap();
        let bytes = rmp_serde::to_vec(&wire).unwrap();
        let error = rmp_serde::from_slice::<Plan>(&bytes).unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
    }
}
