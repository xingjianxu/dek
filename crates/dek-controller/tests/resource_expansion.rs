use std::{collections::BTreeMap, fs};

use dek_controller::modules::{
    DeclaredResource, FileSource, ModuleDefinition, ModuleOutput, ModuleRegistry, ResourceDraft,
    assemble_resources,
};
use dek_core::{
    DesiredResource, DirectoryResource, EffectiveOption, FileAttributes, HostConfig, Origin,
    Priority, ResourceId,
};
use serde_json::json;

fn source(file: &str) -> Origin {
    Origin::Json {
        file: file.into(),
        pointer: "/options/0".parse().unwrap(),
    }
}

fn module(name: &str, file: &str) -> ModuleDefinition {
    ModuleDefinition {
        name: name.into(),
        origin: source(file),
        reads_options: None,
        enabled_by: None,
    }
}

fn file(path: &str, content: FileSource) -> ResourceDraft {
    ResourceDraft::File {
        path: path.into(),
        attributes: FileAttributes::default(),
        content,
    }
}

fn one(draft: ResourceDraft) -> ModuleOutput {
    ModuleOutput {
        resources: vec![DeclaredResource::new(draft)],
        ..ModuleOutput::default()
    }
}

#[test]
fn reports_conflicting_modules_and_sources_for_the_same_target() {
    let root = tempfile::tempdir().unwrap();
    let mut registry = ModuleRegistry::default();
    for (name, origin) in [
        ("left", "profiles/left.json"),
        ("right", "hosts/right.json"),
    ] {
        registry
            .register(module(name, origin), move |_| {
                Ok(one(file(
                    "/etc/test",
                    FileSource::Inline(name.as_bytes().to_vec()),
                )))
            })
            .unwrap();
    }
    let drafts = registry
        .expand(root.path(), &HostConfig::default())
        .unwrap();
    let error = assemble_resources(root.path(), &drafts)
        .unwrap_err()
        .to_string();
    for expected in [
        "left",
        "right",
        "profiles/left.json",
        "hosts/right.json",
        "/etc/test",
    ] {
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn resolves_source_files_relative_to_their_declarations_and_reloads_content() {
    let root = tempfile::tempdir().unwrap();
    let mut registry = ModuleRegistry::default();
    for directory in ["profile", "host"] {
        fs::create_dir(root.path().join(directory)).unwrap();
        fs::write(root.path().join(directory).join("Caddyfile"), directory).unwrap();
        let origin = format!("{directory}/source.json");
        registry
            .register(module(directory, &origin), move |_| {
                Ok(one(file(
                    &format!("/etc/caddy/{directory}"),
                    FileSource::Relative {
                        path: "Caddyfile".into(),
                        origin: source(&origin),
                    },
                )))
            })
            .unwrap();
    }
    let drafts = registry
        .expand(root.path(), &HostConfig::default())
        .unwrap();
    let initial = assemble_resources(root.path(), &drafts).unwrap();
    assert_eq!(initial.contents.len(), 2);
    assert!(initial.contents.values().any(|bytes| bytes == b"profile"));
    assert!(initial.contents.values().any(|bytes| bytes == b"host"));
    assert_eq!(initial.file_inputs.len(), 2);

    fs::write(root.path().join("host/Caddyfile"), b"new host").unwrap();
    let changed = assemble_resources(root.path(), &drafts).unwrap();
    assert_ne!(changed.resources, initial.resources);
    assert!(changed.contents.values().any(|bytes| bytes == b"new host"));
    assert!(changed.contents.values().any(|bytes| bytes == b"profile"));
}

#[test]
fn rejects_dependency_cycles_and_omits_unregistered_resources() {
    let root = tempfile::tempdir().unwrap();
    let mut registry = ModuleRegistry::default();
    for (path, dependency) in [("/a", "/b"), ("/b", "/a")] {
        registry
            .register(module(path, "profiles/base.json"), move |_| {
                let mut resource = DeclaredResource::new(ResourceDraft::Ready(
                    DesiredResource::Directory(DirectoryResource {
                        path: path.into(),
                        attributes: FileAttributes::default(),
                    }),
                ));
                resource
                    .depends_on
                    .push(ResourceId::Path(dependency.into()));
                Ok(ModuleOutput {
                    resources: vec![resource],
                    ..ModuleOutput::default()
                })
            })
            .unwrap();
    }
    let drafts = registry
        .expand(root.path(), &HostConfig::default())
        .unwrap();
    let error = assemble_resources(root.path(), &drafts)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("cycle")
            && error.contains("/a")
            && error.contains("/b")
            && error.contains("profiles/base.json"),
        "{error}"
    );

    let empty = assemble_resources(root.path(), &[]).unwrap();
    assert!(empty.resources.resources.is_empty());
}

#[test]
fn limits_precise_dependencies_and_uses_all_options_as_fallback() {
    let root = tempfile::tempdir().unwrap();
    let effective = |value| EffectiveOption {
        value,
        priority: Priority::Host,
        effective_definitions: vec![],
        overridden_definitions: vec![],
    };
    let config = HostConfig {
        options: BTreeMap::from([
            ("lab.backup.enable".parse().unwrap(), effective(json!(true))),
            (
                "lab.backup.paths".parse().unwrap(),
                effective(json!(["/srv"])),
            ),
            (
                "programs.git.enable".parse().unwrap(),
                effective(json!(true)),
            ),
        ]),
    };
    let mut registry = ModuleRegistry::default();
    let mut precise = module("precise", "modules/precise/options.json");
    precise.reads_options = Some(vec!["lab.backup".parse().unwrap()]);
    registry
        .register(precise, |context| {
            assert_eq!(context.options.options.len(), 2);
            Ok(ModuleOutput::default())
        })
        .unwrap();
    registry
        .register(module("all", "modules/all/options.json"), |context| {
            assert_eq!(context.options.options.len(), 3);
            Ok(ModuleOutput::default())
        })
        .unwrap();
    let mut disabled = module("disabled", "modules/disabled/options.json");
    disabled.enabled_by = Some("lab.other.enable".parse().unwrap());
    registry
        .register(disabled, |_| panic!("module is disabled"))
        .unwrap();
    let mut enabled = module("enabled", "modules/enabled/options.json");
    enabled.enabled_by = Some("lab.backup.enable".parse().unwrap());
    enabled.reads_options = Some(vec![]);
    registry
        .register(enabled, |context| {
            assert!(context.options.options.is_empty());
            Ok(ModuleOutput::default())
        })
        .unwrap();
    let drafts = registry.expand(root.path(), &config).unwrap();
    assert_eq!(drafts.len(), 3);
    assert_eq!(
        drafts
            .iter()
            .find(|unit| unit.definition.name == "precise")
            .unwrap()
            .options
            .options
            .len(),
        2
    );
    assert_eq!(
        drafts
            .iter()
            .find(|unit| unit.definition.name == "all")
            .unwrap()
            .options
            .options
            .len(),
        3
    );
    assert!(
        assemble_resources(root.path(), &drafts)
            .unwrap()
            .resources
            .resources
            .is_empty()
    );
}

#[test]
fn coalesces_equal_resources_without_losing_contributors_or_notifications() {
    use dek_core::{
        NotifyWhen, ServiceEffect, ServiceNotification, SystemdUnitResource, UnitDefinition,
        UnitId, UnitScope,
    };

    let root = tempfile::tempdir().unwrap();
    let mut registry = ModuleRegistry::default();
    let unit = UnitId {
        scope: UnitScope::System,
        name: "backup.timer".into(),
    };

    for (name, scope) in [
        ("first", UnitScope::System),
        ("second", UnitScope::System),
        (
            "user",
            UnitScope::User {
                user: "alice".into(),
            },
        ),
    ] {
        let target = unit.clone();
        registry
            .register(module(name, "profiles/base.json"), move |_| {
                let mut output = one(ResourceDraft::Ready(DesiredResource::SystemdUnit(
                    SystemdUnitResource {
                        unit: UnitId {
                            scope: scope.clone(),
                            name: target.name.clone(),
                        },
                        definition: UnitDefinition::default(),
                        enabled: Some(true),
                        running: None,
                        activates: None,
                    },
                )));
                if name != "user" {
                    let mut config = DeclaredResource::new(file(
                        "/etc/backup",
                        FileSource::Inline(b"config".to_vec()),
                    ));
                    config.notifications.push(ServiceNotification {
                        unit: target.clone(),
                        effect: ServiceEffect::Reload,
                        when: NotifyWhen::ContentChanged,
                    });
                    output.resources.push(config);
                }
                Ok(output)
            })
            .unwrap();
    }
    let drafts = registry
        .expand(root.path(), &HostConfig::default())
        .unwrap();
    let result = assemble_resources(root.path(), &drafts).unwrap();
    assert_eq!(result.resources.resources.len(), 3);
    let resources = &result.resources.resources;
    let file = resources
        .iter()
        .find(|resource| resource.id() == ResourceId::Path("/etc/backup".into()))
        .unwrap();
    assert_eq!(file.origins.len(), 2);
    assert_eq!(file.notifications.len(), 1);
    let system = resources
        .iter()
        .find(|resource| resource.id() == ResourceId::SystemdUnit(unit.clone()))
        .unwrap();
    assert_eq!(system.origins.len(), 2);
}
