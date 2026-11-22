use std::path::Path;

use dek_controller::modules::{
    ExpandedResources, ModuleRegistry, SchemaRegistry, assemble_resources, register_system_modules,
    register_system_schemas,
};
use dek_core::{
    DesiredResource, OptionAssignment, Origin, Resource, ResourceId, UnitId, UnitScope, UnitSpec,
    UserKind,
};
use serde_json::{Value, json};

fn declarations(options: Value) -> UnitSpec {
    let options = options.as_array().unwrap().iter().enumerate().map(|(index, option)| {
        let mut option = option.clone();
        option["origin"] = json!({"file": "hosts/server/default.json", "pointer": format!("/options/{index}")});
        serde_json::from_value::<OptionAssignment>(option).unwrap()
    }).collect();
    UnitSpec {
        options,
        ..UnitSpec::default()
    }
}

fn expand(profiles: &[UnitSpec], host: &UnitSpec) -> Result<ExpandedResources, String> {
    let mut schemas = SchemaRegistry::default();
    register_system_schemas(&mut schemas).unwrap();
    let mut modules = ModuleRegistry::default();
    register_system_modules(&mut modules).unwrap();
    let config = schemas
        .merge(profiles, host)
        .map_err(|err| err.to_string())?;
    let root = Path::new(".");
    let drafts = modules
        .expand(root, &config)
        .map_err(|err| err.to_string())?;
    assemble_resources(root, &drafts).map_err(|err| err.to_string())
}

fn resource(expanded: &ExpandedResources, id: ResourceId) -> &Resource {
    expanded
        .resources
        .resources
        .iter()
        .find(|resource| resource.id() == id)
        .unwrap_or_else(|| panic!("missing {id:?}"))
}

fn unit_id(scope: UnitScope, name: &str) -> UnitId {
    UnitId {
        scope,
        name: name.into(),
    }
}

#[test]
fn json_accounts_and_directory_preserve_attributes_and_managed_dependencies() {
    let host = declarations(json!([
        {"path":"users.groups.research", "value":{}},
        {"path":"users.users.alice.isNormalUser", "value":true},
        {"path":"users.users.alice.group", "value":"research"},
        {"path":"users.users.alice.extraGroups", "value":["research", "wheel"]},
        {"path":"users.users.alice.shell", "value":"/bin/bash"},
        {"path":"systemd.tmpfiles.settings.dek", "value":{"/srv/research":{"d":{
            "user":"alice", "group":"research", "mode":"0750"
        }}}}
    ]));
    let set = expand(&[], &host).unwrap();
    assert_eq!(set.resources.resources.len(), 3);

    let group = resource(&set, ResourceId::Group("research".into()));
    assert!(matches!(&group.desired, DesiredResource::Group(group) if group.gid.is_none()));

    let user = resource(&set, ResourceId::User("alice".into()));
    let DesiredResource::User(account) = &user.desired else {
        unreachable!()
    };
    assert_eq!(account.kind, Some(UserKind::Normal));
    assert_eq!(account.uid, None);
    assert_eq!(account.home, None);
    assert_eq!(account.group.as_deref(), Some("research"));
    assert_eq!(account.shell.as_deref(), Some("/bin/bash"));
    assert_eq!(
        account.extra_groups.as_deref().unwrap(),
        ["research", "wheel"]
    );
    assert_eq!(user.depends_on, [ResourceId::Group("research".into())]);

    let directory = resource(&set, ResourceId::Path("/srv/research".into()));
    let DesiredResource::Directory(desired) = &directory.desired else {
        unreachable!()
    };
    assert_eq!(desired.attributes.owner.as_deref(), Some("alice"));
    assert_eq!(desired.attributes.group.as_deref(), Some("research"));
    assert_eq!(desired.attributes.mode.as_deref(), Some("0750"));
    assert!(directory.depends_on.contains(&user.id()));
    assert!(directory.depends_on.contains(&group.id()));
    assert_eq!(directory.origins[0].origin, host.options[5].origin);
}

#[test]
fn named_scopes_keep_equal_unit_names_distinct_and_add_declared_account_edges() {
    let host = declarations(json!([
        {"path":"users.users.alice.isNormalUser", "value":true},
        {"path":"users.users.bob.isSystemUser", "value":true},
        {"path":"systemd.timers", "value":{"backup.timer":{"enabled":false}}},
        {"path":"systemd.user.timers", "value":{"backup.timer":{"running":true}}},
        {"path":"dek.users.alice.systemd.timers", "value":{"backup.timer":{"enabled":true}}},
        {"path":"dek.users.bob.systemd.timers", "value":{"backup.timer":{"running":false}}}
    ]));
    let set = expand(&[], &host).unwrap();
    for (scope, enabled, running, dependency) in [
        (UnitScope::System, Some(false), None, None),
        (UnitScope::UserGlobal, None, Some(true), None),
        (
            UnitScope::User {
                user: "alice".into(),
            },
            Some(true),
            None,
            Some("alice"),
        ),
        (
            UnitScope::User { user: "bob".into() },
            None,
            Some(false),
            Some("bob"),
        ),
    ] {
        let id = unit_id(scope, "backup.timer");
        let found = resource(&set, ResourceId::SystemdUnit(id.clone()));
        let DesiredResource::SystemdUnit(unit) = &found.desired else {
            unreachable!()
        };
        assert_eq!(unit.unit, id);
        assert_eq!((unit.enabled, unit.running), (enabled, running));
        assert_eq!(
            found.depends_on,
            dependency
                .map(|name| ResourceId::User(name.into()))
                .into_iter()
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn oneshot_service_is_deployed_without_runtime_state_and_timer_uses_managed_service() {
    let host = declarations(json!([
        {"path":"systemd.services", "value":{"backup.service":{
            "description":"Backup once", "serviceConfig":{"Type":"oneshot", "ExecStart":"/usr/bin/backup"}
        }}},
        {"path":"systemd.timers", "value":{"nightly.timer":{
            "timerConfig":{"OnCalendar":"daily", "Unit":"backup.service"},
            "wantedBy":["timers.target"], "enabled":true, "running":true
        }}}
    ]));
    let set = expand(&[], &host).unwrap();
    let service_id = unit_id(UnitScope::System, "backup.service");
    let service = resource(&set, ResourceId::SystemdUnit(service_id.clone()));
    let timer = resource(
        &set,
        ResourceId::SystemdUnit(unit_id(UnitScope::System, "nightly.timer")),
    );
    let DesiredResource::SystemdUnit(service_data) = &service.desired else {
        unreachable!()
    };
    let DesiredResource::SystemdUnit(timer_data) = &timer.desired else {
        unreachable!()
    };
    let service_text =
        String::from_utf8_lossy(&set.contents[service_data.definition.content.as_ref().unwrap()]);
    let timer_text =
        String::from_utf8_lossy(&set.contents[timer_data.definition.content.as_ref().unwrap()]);
    assert!(
        service_text.contains("[Service]\n") && service_text.contains("Type=oneshot\n"),
        "{service_text:?}"
    );
    assert!(
        timer_text.contains("Unit=backup.service\n")
            && timer_text.contains("WantedBy=timers.target\n"),
        "{timer_text:?}"
    );
    assert_eq!((service_data.enabled, service_data.running), (None, None));
    assert_eq!(
        (timer_data.enabled, timer_data.running),
        (Some(true), Some(true))
    );
    assert_eq!(timer_data.activates.as_ref(), Some(&service_id));
    assert_eq!(timer.depends_on, [service.id()]);
}

#[test]
fn directory_maps_merge_by_path_and_attribute_each_members_winning_source() {
    let profile = declarations(json!([
        {"path":"systemd.tmpfiles.settings.dek", "value":{
            "/srv/archive.v1":{"d":{"mode":"0700"}}, "/srv/cache":{"d":{"mode":"0750"}}
        }},
        {"path":"systemd.tmpfiles.settings.dek", "value":{"/srv/spare":{"d":{}}}}
    ]));
    let mut host = declarations(json!([
        {"path":"systemd.tmpfiles.settings.dek", "value":{"/srv/archive.v1":{"d":{"mode":"0755"}}}}
    ]));
    host.options[0].origin = Origin::Ruby {
        file: "hosts/server/default.rb".into(),
        line: 7.try_into().unwrap(),
    };
    let set = expand(std::slice::from_ref(&profile), &host).unwrap();
    assert_eq!(set.resources.resources.len(), 3);
    for (name, mode, origin) in [
        ("/srv/archive.v1", Some("0755"), &host.options[0].origin),
        ("/srv/cache", Some("0750"), &profile.options[0].origin),
        ("/srv/spare", None, &profile.options[1].origin),
    ] {
        let found = resource(&set, ResourceId::Path(name.into()));
        let DesiredResource::Directory(directory) = &found.desired else {
            unreachable!()
        };
        assert_eq!(directory.attributes.mode.as_deref(), mode);
        assert_eq!(found.origins.len(), 1);
        assert_eq!(&found.origins[0].origin, origin);
    }
}

#[test]
fn reports_invalid_fields_and_preserves_unmanaged_references_without_edges() {
    assert!(
        expand(&[], &UnitSpec::default())
            .unwrap()
            .resources
            .resources
            .is_empty()
    );
    for (option, expected) in [
        (
            json!({"path":"users.groups.shared.gid","value":-1}),
            "users.groups.shared.gid",
        ),
        (
            json!({"path":"systemd.tmpfiles.settings.dek","value":{"/srv/a":{"d":{"mode":755}}}}),
            "/~1srv~1a/d/mode",
        ),
        (
            json!({"path":"systemd.services","value":{"test.service":{"running":"yes"}}}),
            "running",
        ),
    ] {
        let error = expand(&[], &declarations(json!([option]))).unwrap_err();
        assert!(
            error.contains(expected) && error.contains("/options/0"),
            "{error}"
        );
    }
    let host = declarations(json!([
        {"path":"systemd.tmpfiles.settings.dek","value":{"/srv/data":{"d":{"user":"existing-user","group":"existing-group"}}}},
        {"path":"dek.users.existing-user.systemd.services","value":{"existing.service":{"enabled":false}}}
    ]));
    let set = expand(&[], &host).unwrap();
    assert_eq!(set.resources.resources.len(), 2);
    assert!(
        set.resources
            .resources
            .iter()
            .all(|resource| resource.depends_on.is_empty())
    );
}

#[test]
fn directory_conflicts_name_both_calls_and_positional_rules_are_invalid() {
    let mut conflicting = declarations(json!([
        {"path":"systemd.tmpfiles.settings.dek","value":{"/srv/shared":{"d":{"mode":"0750"}}}},
        {"path":"systemd.tmpfiles.settings.dek","value":{"/srv/shared":{"d":{"mode":"0700"}}}}
    ]));
    conflicting.options[1].origin = Origin::Ruby {
        file: "hosts/server/default.rb".into(),
        line: 42.try_into().unwrap(),
    };
    let error = expand(&[], &conflicting).unwrap_err();
    for expected in ["/srv/shared", "/options/0", "hosts/server/default.rb:42"] {
        assert!(error.contains(expected), "{error}");
    }

    let malformed = declarations(json!([
        {"path":"systemd.tmpfiles.settings.dek","value":{"/srv/array":{"d":[null,null,"0750"]}}}
    ]));
    let error = expand(&[], &malformed).unwrap_err();
    assert!(
        error.contains("/~1srv~1array/d") && error.contains("/options/0"),
        "{error}"
    );
}

#[test]
fn applies_explicit_account_fields_and_reports_conflicting_kinds() {
    let mut host = declarations(json!([
        {"path":"users.groups.research.gid","value":1200},
        {"path":"users.users.alice.isSystemUser","value":true},
        {"path":"users.users.alice.isNormalUser","value":false},
        {"path":"users.users.alice.uid","value":1201},
        {"path":"users.users.alice.home","value":"/var/lib/backup"},
        {"path":"users.users.alice.description","value":"Backup job"},
        {"path":"users.users.alice.extraGroups","value":[]}
    ]));
    let expanded = expand(&[], &host).unwrap();
    let group = resource(&expanded, ResourceId::Group("research".into()));
    assert!(matches!(&group.desired, DesiredResource::Group(group) if group.gid == Some(1200)));
    let user = resource(&expanded, ResourceId::User("alice".into()));
    let DesiredResource::User(user) = &user.desired else {
        unreachable!()
    };
    assert_eq!(user.kind, Some(UserKind::System));
    assert_eq!(user.uid, Some(1201));
    assert_eq!(user.extra_groups.as_deref(), Some([].as_slice()));
    assert_eq!(user.home.as_deref(), Some("/var/lib/backup"));
    assert_eq!(user.description.as_deref(), Some("Backup job"));

    host.options[2].value = json!(true);
    let error = expand(&[], &host).unwrap_err();
    for expected in [
        "users.users.alice",
        "isNormalUser",
        "isSystemUser",
        "/options/1",
        "/options/2",
    ] {
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn canonical_unit_keys_are_required_and_merge_across_profiles() {
    let mut profile = declarations(json!([
        {"path": "systemd.timers", "value": {"backup": {"enabled": true}}}
    ]));
    let host = declarations(json!([
        {"path": "systemd.timers", "value": {"backup.timer": {"enabled": false}}}
    ]));
    profile.options[0].origin = Origin::Ruby {
        file: "profiles/schedule.rb".into(),
        line: 4.try_into().unwrap(),
    };
    let error = expand(std::slice::from_ref(&profile), &host).unwrap_err();
    for part in ["backup", ".timer suffix", "profiles/schedule.rb:4"] {
        assert!(error.contains(part), "{error}");
    }

    profile.options[0].value = json!({"backup.timer": {"enabled": true}});
    let result = expand(&[profile], &host).unwrap();
    let [timer] = &result.resources.resources[..] else {
        panic!("expected only one timer");
    };
    assert!(
        matches!(&timer.desired, DesiredResource::SystemdUnit(unit) if unit.enabled == Some(false))
    );
    assert_eq!(timer.origins[0].origin, host.options[0].origin);
}

#[test]
fn system_service_depends_on_managed_user_and_group_named_in_directives() {
    let host = declarations(json!([
        {"path":"users.users.worker", "value":{}},
        {"path":"users.groups.workers", "value":{}},
        {"path":"systemd.services", "value":{
            "backup.service":{"serviceConfig":{"User":"worker","Group":"workers"}},
            "unmanaged.service":{"serviceConfig":{"User":"external","Group":"external"}}
        }}
    ]));
    let set = expand(&[], &host).unwrap();
    let service = resource(
        &set,
        ResourceId::SystemdUnit(unit_id(UnitScope::System, "backup.service")),
    );
    assert_eq!(
        service.depends_on,
        [
            ResourceId::User("worker".into()),
            ResourceId::Group("workers".into())
        ]
    );
    let external = resource(
        &set,
        ResourceId::SystemdUnit(unit_id(UnitScope::System, "unmanaged.service")),
    );
    assert!(external.depends_on.is_empty());

    let hidden = declarations(json!([
        {"path":"systemd.services", "value":{"backup.service":{
            "dropIns":{"account.conf":"[Service]\nGroup=workers\n"}
        }}}
    ]));
    let error = expand(&[], &hidden).unwrap_err();
    for part in [
        "account.conf",
        "User or Group",
        "serviceConfig",
        "/options/0",
    ] {
        assert!(error.contains(part), "{error}");
    }
}

#[test]
fn rendered_unit_directives_cannot_consume_the_next_line() {
    let host = declarations(json!([
        {"path":"systemd.services", "value":{"backup.service":{"serviceConfig":{
            "ExecStart":r"/usr/bin/backup\", "Type":"oneshot"
        }}}}
    ]));
    let error = expand(&[], &host).unwrap_err();
    for part in ["Service.ExecStart", "backslash", "/options/0"] {
        assert!(error.contains(part), "{error}");
    }
}

#[test]
fn timer_drop_ins_cannot_override_the_declared_service_binding() {
    for override_text in [
        "[Timer]\nUnit=other.service\n",
        "[Timer]\nUnit \\\n# continued assignment\n\n =other.service\n",
    ] {
        let host = declarations(json!([
            {"path":"systemd.timers", "value":{"backup.timer":{
                "timerConfig":{"Unit":"backup.service"},
                "dropIns":{"override.conf":override_text}
            }}}
        ]));
        let error = expand(&[], &host).unwrap_err();
        for part in [
            "backup.timer",
            "override.conf",
            "timerConfig.Unit",
            "/options/0",
        ] {
            assert!(error.contains(part), "{error}");
        }
    }

    let host = declarations(json!([
        {"path":"systemd.timers", "value":{"backup.timer":{
            "dropIns":{"schedule.conf":"[Timer]\n# Unit=other.service\nOnCalendar=weekly\n"}
        }}}
    ]));
    let output = expand(&[], &host).unwrap();
    let timer = resource(
        &output,
        ResourceId::SystemdUnit(unit_id(UnitScope::System, "backup.timer")),
    );
    assert!(matches!(&timer.desired, DesiredResource::SystemdUnit(unit)
        if unit.activates == Some(unit_id(UnitScope::System, "backup.service"))
        && unit.definition.drop_ins.contains_key("schedule.conf")));
}
