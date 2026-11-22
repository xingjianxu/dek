use std::{fs, path::Path};

use dek_controller::{
    eval::{RubySession, TargetAnswer, TargetProvider},
    modules::{ModuleRegistry, RepositoryModules, SchemaRegistry, assemble_resources},
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
            {"path": "lab.backup.day", "type": "string", "description": "定时日"}
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
return_value = option("lab.backup.day")
file_resource "/etc/lab-backup.conf", content: "day=#{return_value}"
resource :systemdUnit, unit: {scope: "system", name: "lab-backup.service"}
resource :systemdUnit,
  unit: {scope: "system", name: "lab-backup.timer"}, enabled: true, running: true
"#,
    );
    let mut schemas = SchemaRegistry::default();
    let custom = RepositoryModules::load(&repo, &mut schemas).unwrap();
    assert_eq!(custom.input_policy("lab.backup"), Some(InputPolicy::Sdk));
    let mut runtime = RubySession::new();
    let mut target = UnusedTarget;
    let mut modules = ModuleRegistry::default();
    custom
        .register(&mut modules, &mut runtime, &mut target)
        .unwrap();
    let off = schemas.merge(&[], &UnitSpec::default()).unwrap();
    assert!(modules.expand(root.path(), &off).unwrap().is_empty());

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
    assert!(
        expanded[0]
            .output
            .inputs
            .iter()
            .any(|input| input.kind == InputKind::File && input.id == "modules/lab/expand.rb")
    );
    let result = assemble_resources(root.path(), &expanded).unwrap();
    assert_eq!(result.resources.resources.len(), 3);
    assert_eq!(
        result.contents.values().next().unwrap().as_slice(),
        b"day=Fri"
    );
    let file = result
        .resources
        .resources
        .iter()
        .find(|resource| resource.id() == ResourceId::Path("/etc/lab-backup.conf".into()))
        .unwrap();
    assert_eq!(file.origins[0].module, "lab.backup");
    assert!(file.origins[0].origin.to_string().contains("expand.rb:3"));
}

#[test]
fn duplicate_schema_and_invalid_expansion_name_both_sources() {
    let root = tempfile::tempdir().unwrap();
    let repo = fixture(
        root.path(),
        None,
        r#"resource :directory, path: "/tmp/backup", unknownField: true"#,
    );
    let mut schemas = SchemaRegistry::default();
    schemas
        .register_json(
            "builtin",
            "builtins/backup.json",
            &json!({"options": [
                {"path": "lab.backup.enable", "type": "boolean", "description": "Built-in"}
            ]})
            .to_string(),
        )
        .unwrap();
    let duplicate = RepositoryModules::load(&repo, &mut schemas)
        .unwrap_err()
        .to_string();
    assert!(
        duplicate.contains("builtins/backup.json")
            && duplicate.contains("modules/lab/options.json"),
        "{duplicate}"
    );

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
        .register(&mut registry, &mut runtime, &mut target)
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
            .register(&mut registry, &mut runtime, &mut target)
            .unwrap();
        let expanded = registry.expand(root.path(), &config).unwrap();
        assert_eq!(expanded[0].options.options.len(), expected_count);
        let files = assemble_resources(root.path(), &expanded).unwrap().contents;
        let data = std::str::from_utf8(files.values().next().unwrap()).unwrap();
        assert_eq!(data.contains("programs.git.enable"), expected_count == 3);
    }
}
