use std::{fs, path::Path};

use dek_controller::{
    eval::{RubyProviders, RubySession, TargetAnswer, TargetProvider},
    modules::{
        ModuleRegistry, RepositoryModules, SchemaRegistry, assemble_resources, register_app_schemas,
    },
    repo::{InputPolicy, Repository},
};
use dek_core::{InputKind, OptionAssignment, Origin, ResourceId, UnitSpec, protocol::TargetQuery};
use serde_json::{Value, json};

struct UnusedTarget;

impl TargetProvider for UnusedTarget {
    fn query(&mut self, query: &TargetQuery) -> Result<TargetAnswer, String> {
        panic!("unexpected target query: {query:?}")
    }
}

fn fixture(root: &Path, reads_options: Option<Value>, expansion: &str) -> Repository {
    let mut manifest = json!({
        "schemaVersion": 1,
        "hosts": {},
        "modules": { "lab.backup": {
            "schema": "modules/lab/options.json", "expand": "modules/lab/expand.rb", "inputs": "sdk"
        }}
    });
    if let Some(paths) = reads_options {
        manifest["modules"]["lab.backup"]["readsOptions"] = paths;
    }
    fs::create_dir_all(root.join("modules/lab")).unwrap();
    fs::write(root.join("dek.json"), manifest.to_string()).unwrap();
    fs::write(
        root.join("modules/lab/options.json"),
        json!({"options": [
            {"path": "lab.backup.enable", "type": "boolean", "default": false, "description": "启用"},
            {"path": "lab.backup.day", "type": "string", "description": "定时日"},
            {"path": "lab.backup.template", "type": "sourceFile", "description": "模板"}
        ]}).to_string(),
    ).unwrap();
    fs::write(root.join("modules/lab/expand.rb"), expansion).unwrap();
    Repository::load(root).unwrap()
}

fn assignment(path: &str, value: Value, file: &str) -> OptionAssignment {
    OptionAssignment {
        path: path.parse().unwrap(),
        value,
        origin: Origin::Json {
            file: file.into(),
            pointer: "/options/0".parse().unwrap(),
        },
    }
}

#[test]
fn custom_modules_use_effective_options_and_preserve_resource_sources() {
    let root = tempfile::tempdir().unwrap();
    let repo = fixture(
        root.path(),
        Some(json!(["lab.backup"])),
        r#"
require_relative "helper"
return_value = [option("lab.backup.day"), read_text("note")].join
file_resource "/etc/lab-backup.conf", content: "day=#{return_value}"
resource :systemdUnit, unit: {scope: "system", name: "lab-backup.service"}
resource :systemdUnit,
  unit: {scope: "system", name: "lab-backup.timer"}, enabled: true, running: true
"#,
    );
    fs::write(
        root.path().join("modules/lab/helper.rb"),
        "# require fixture",
    )
    .unwrap();
    fs::write(root.path().join("modules/lab/note"), ":ok").unwrap();
    let mut schemas = SchemaRegistry::default();
    let custom = RepositoryModules::load(&repo, &mut schemas).unwrap();
    assert_eq!(custom.input_policy("lab.backup"), Some(InputPolicy::Sdk));
    let mut runtime = RubySession::new();
    let mut target = UnusedTarget;
    let mut modules = ModuleRegistry::default();
    custom
        .register(
            &mut modules,
            &mut runtime,
            RubyProviders {
                target: &mut target,
                http: None,
            },
        )
        .unwrap();
    let off = schemas.merge(&[], &UnitSpec::default()).unwrap();
    let entry = root.path().join("modules/lab/expand.rb");
    let disabled_entry = entry.with_extension("disabled");
    fs::rename(&entry, &disabled_entry).unwrap();
    assert!(modules.expand(root.path(), &off).unwrap().is_empty());
    fs::rename(disabled_entry, entry).unwrap();

    let profile = UnitSpec {
        options: vec![assignment("lab.backup.day", json!("Mon"), "base.json")],
        ..UnitSpec::default()
    };
    let host = UnitSpec {
        options: vec![
            assignment("lab.backup.enable", json!(true), "host.json"),
            assignment("lab.backup.day", json!("Fri"), "host.json"),
        ],
        ..UnitSpec::default()
    };
    let on = schemas.merge(&[profile], &host).unwrap();
    let expanded = modules.expand(root.path(), &on).unwrap();
    assert_eq!(expanded[0].options.options.len(), 2);
    for path in ["expand.rb", "helper.rb", "note"] {
        assert!(
            expanded[0]
                .output
                .inputs
                .iter()
                .any(|input| input.kind == InputKind::File
                    && input.id == format!("modules/lab/{path}")),
            "missing input {path}"
        );
    }
    let result = assemble_resources(root.path(), &expanded).unwrap();
    assert_eq!(result.resources.resources.len(), 3);
    assert_eq!(
        result.contents.values().next().unwrap().as_slice(),
        b"day=Fri:ok"
    );
    let file = result
        .resources
        .resources
        .iter()
        .find(|resource| resource.id() == ResourceId::Path("/etc/lab-backup.conf".into()))
        .unwrap();
    assert_eq!(file.origins[0].module, "lab.backup");
    assert!(file.origins[0].origin.to_string().contains("expand.rb:4"));
}

#[test]
fn duplicate_schema_and_invalid_expansion_name_both_sources() {
    let root = tempfile::tempdir().unwrap();
    let repo = fixture(
        root.path(),
        None,
        r#"resource :directory, path: "/tmp/backup", unknownField: true"#,
    );
    let schema_path = root.path().join("modules/lab/options.json");
    let original_schema = fs::read(&schema_path).unwrap();
    fs::write(
        &schema_path,
        json!({"options": [
            {"path": "services.caddy.enable", "type": "boolean", "description": "重复"}
        ]})
        .to_string(),
    )
    .unwrap();
    let mut schemas = SchemaRegistry::default();
    register_app_schemas(&mut schemas).unwrap();
    let duplicate = RepositoryModules::load(&repo, &mut schemas)
        .unwrap_err()
        .to_string();
    assert!(
        duplicate.contains("builtin/apps/options.json")
            && duplicate.contains("modules/lab/options.json"),
        "{duplicate}"
    );
    fs::write(schema_path, original_schema).unwrap();

    let mut schemas = SchemaRegistry::default();
    let custom = RepositoryModules::load(&repo, &mut schemas).unwrap();
    let config = schemas
        .merge(
            &[],
            &UnitSpec {
                options: vec![assignment("lab.backup.enable", json!(true), "host.json")],
                ..UnitSpec::default()
            },
        )
        .unwrap();
    let mut runtime = RubySession::new();
    let mut target = UnusedTarget;
    let mut registry = ModuleRegistry::default();
    custom
        .register(
            &mut registry,
            &mut runtime,
            RubyProviders {
                target: &mut target,
                http: None,
            },
        )
        .unwrap();
    let error = registry
        .expand(root.path(), &config)
        .unwrap_err()
        .to_string();
    for expected in ["expand", "lab.backup", "expand.rb", "unknownField"] {
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn custom_access_is_scoped_to_reads_options() {
    let root = tempfile::tempdir().unwrap();
    for (paths, expected_count) in [(Some(json!(["lab.backup"])), 2), (None, 3)] {
        let repo = fixture(
            root.path(),
            paths,
            r#"file_resource "/etc/options", content: options.keys.sort.join(",")"#,
        );
        let mut schemas = SchemaRegistry::default();
        schemas.register_json("builtin", "builtins/git.json", &json!({"options": [
            {"path": "programs.git.enable", "type": "boolean", "default": true, "description": "Git"}
        ]}).to_string()).unwrap();
        let custom = RepositoryModules::load(&repo, &mut schemas).unwrap();
        let config = schemas
            .merge(
                &[],
                &UnitSpec {
                    options: vec![
                        assignment("lab.backup.enable", json!(true), "host.json"),
                        assignment("lab.backup.day", json!("Mon"), "host.json"),
                    ],
                    ..UnitSpec::default()
                },
            )
            .unwrap();
        let mut runtime = RubySession::new();
        let mut target = UnusedTarget;
        let mut registry = ModuleRegistry::default();
        custom
            .register(
                &mut registry,
                &mut runtime,
                RubyProviders {
                    target: &mut target,
                    http: None,
                },
            )
            .unwrap();
        let expanded = registry.expand(root.path(), &config).unwrap();
        assert_eq!(expanded[0].options.options.len(), expected_count);
        let files = assemble_resources(root.path(), &expanded).unwrap().contents;
        let data = std::str::from_utf8(files.values().next().unwrap()).unwrap();
        assert_eq!(data.contains("programs.git.enable"), expected_count == 3);
    }
}

#[test]
fn source_option_uses_the_winning_declaration_directory() {
    let root = tempfile::tempdir().unwrap();
    let repo = fixture(
        root.path(),
        None,
        r#"file_resource "/etc/lab", source: option_source("lab.backup.template")"#,
    );
    fs::create_dir(root.path().join("hosts")).unwrap();
    fs::write(root.path().join("hosts/template"), "winning host").unwrap();
    fs::write(root.path().join("modules/lab/template"), "wrong directory").unwrap();
    let mut schemas = SchemaRegistry::default();
    let custom = RepositoryModules::load(&repo, &mut schemas).unwrap();
    let config = schemas
        .merge(
            &[],
            &UnitSpec {
                options: vec![
                    assignment("lab.backup.enable", json!(true), "hosts/server.json"),
                    assignment(
                        "lab.backup.template",
                        json!("template"),
                        "hosts/server.json",
                    ),
                ],
                ..UnitSpec::default()
            },
        )
        .unwrap();
    let mut runtime = RubySession::new();
    let mut target = UnusedTarget;
    let mut registry = ModuleRegistry::default();
    custom
        .register(
            &mut registry,
            &mut runtime,
            RubyProviders {
                target: &mut target,
                http: None,
            },
        )
        .unwrap();
    let expanded = registry.expand(root.path(), &config).unwrap();
    let result = assemble_resources(root.path(), &expanded).unwrap();
    assert_eq!(result.contents.values().next().unwrap(), b"winning host");
    assert!(
        result.resources.resources[0]
            .origins
            .iter()
            .any(|origin| origin.origin.file() == "hosts/server.json")
    );
}
