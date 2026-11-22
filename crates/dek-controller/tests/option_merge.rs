use dek_controller::modules::SchemaRegistry;
use dek_core::{OptionAssignment, OptionPath, Origin, Priority, UnitSpec};
use serde_json::{Value, json};

fn assignment(path: &str, value: Value, file: &str, line: u32) -> OptionAssignment {
    OptionAssignment {
        path: path.parse().unwrap(),
        value,
        origin: Origin::Ruby {
            file: file.into(),
            line: line.try_into().unwrap(),
        },
    }
}

fn unit(options: Vec<OptionAssignment>) -> UnitSpec {
    UnitSpec {
        options,
        ..UnitSpec::default()
    }
}

fn registry(options: Value) -> SchemaRegistry {
    let mut registry = SchemaRegistry::default();
    registry
        .register_json(
            "test",
            "modules/test/options.json",
            &json!({"options": options}).to_string(),
        )
        .unwrap();
    registry
}

fn path(path: &str) -> OptionPath {
    path.parse().unwrap()
}

#[test]
fn scalar_priority_retains_default_profile_and_host_definitions() {
    let schema = registry(json!([{
        "path": "services.caddy.enable", "type": "boolean",
        "default": false, "description": "Enable Caddy"
    }]));
    let name = "services.caddy.enable";
    let profile = assignment(name, json!(true), "profiles/base.rb", 2);
    let host = assignment(name, json!(false), "hosts/server/default.rb", 5);
    let config = schema
        .merge(&[unit(vec![profile.clone()])], &unit(vec![host.clone()]))
        .unwrap();
    let option = &config.options[&path(name)];
    assert_eq!(option.value, false);
    assert_eq!(option.priority, Priority::Host);
    assert_eq!(option.effective_definitions.len(), 1);
    assert_eq!(option.effective_definitions[0].origin, host.origin);
    assert_eq!(option.overridden_definitions.len(), 2);
    let default = &option.overridden_definitions[0];
    assert_eq!(default.priority, Priority::ModuleDefault);
    assert_eq!(default.value, false);
    assert_eq!(default.origin.file(), "modules/test/options.json");
    assert!(default.origin.to_string().contains("/options/0/default"));
    assert_eq!(option.overridden_definitions[1].priority, Priority::Profile);
    assert_eq!(option.overridden_definitions[1].origin, profile.origin);
    assert_eq!(option.overridden_definitions[1].value, true);

    let empty = schema.merge(&[], &UnitSpec::default()).unwrap();
    assert_eq!(empty.options[&path(name)].priority, Priority::ModuleDefault);
}

#[test]
fn equal_same_priority_scalars_coalesce_and_conflicts_name_both_sources() {
    let schema = registry(json!([{
        "path": "services.caddy.enable", "type": "boolean",
        "description": "Enable Caddy"
    }]));
    let name = "services.caddy.enable";
    let first = assignment(name, json!(true), "profiles/first.rb", 4);
    let second = assignment(name, json!(true), "profiles/second.rb", 7);
    let same = schema
        .merge(
            &[unit(vec![first.clone()]), unit(vec![second.clone()])],
            &UnitSpec::default(),
        )
        .unwrap();
    assert_eq!(same.options[&path(name)].effective_definitions.len(), 2);

    let conflict = OptionAssignment {
        value: json!(false),
        ..second
    };
    let error = schema
        .merge(
            &[unit(vec![first]), unit(vec![conflict])],
            &UnitSpec::default(),
        )
        .unwrap_err();
    for text in [name, "profiles/first.rb:4", "profiles/second.rb:7"] {
        assert!(error.to_string().contains(text), "{error}");
    }
}

#[test]
fn sets_deduplicate_and_ordered_lists_preserve_all_elements_across_layers() {
    let schema = registry(json!([
        {"path": "environment.systemPackages", "type": {"list": "string"}, "merge": "set",
         "default": ["base"], "description": "Packages"},
        {"path": "test.sequence", "type": {"list": "string"}, "merge": "ordered",
         "default": ["base"], "description": "Sequence"}
    ]));
    let packages = "environment.systemPackages";
    let sequence = "test.sequence";
    let profile = unit(vec![
        assignment(
            packages,
            json!(["z", "base", "a", "z"]),
            "profiles/base.rb",
            2,
        ),
        assignment(
            sequence,
            json!(["z", "base", "a", "z"]),
            "profiles/base.rb",
            3,
        ),
    ]);
    let host = unit(vec![
        assignment(packages, json!(["a", "host"]), "hosts/server.rb", 2),
        assignment(sequence, json!(["a", "host"]), "hosts/server.rb", 3),
    ]);
    let config = schema.merge(&[profile], &host).unwrap();
    let set = &config.options[&path(packages)];
    assert_eq!(set.value, json!(["base", "z", "a", "host"]));
    assert_eq!(set.priority, Priority::Host);
    assert_eq!(set.effective_definitions.len(), 3);
    assert!(set.overridden_definitions.is_empty());
    let ordered = &config.options[&path(sequence)];
    assert_eq!(
        ordered.value,
        json!(["base", "z", "base", "a", "z", "a", "host"])
    );
    assert_eq!(ordered.effective_definitions.len(), 3);
    assert!(ordered.overridden_definitions.is_empty());
}

#[test]
fn validates_named_subtrees_and_preserves_free_form_map_keys() {
    let schema = registry(json!([
        {"path": "users.users.<name>.extraGroups", "type": {"list": "string"},
         "merge": "set", "description": "Supplementary groups"},
        {"path": "programs.git.config", "type": {"map": "any"}, "description": "Git config"}
    ]));
    let accounts = "users.users.alice.extraGroups";
    let git = "programs.git.config";
    let value = json!({"init": {"defaultBranch": "main"}, "includeIf.gitdir:~/Project/": {"path": "Git.inc"}});
    let host = unit(vec![
        assignment(accounts, json!(["research"]), "hosts/server.rb", 3),
        assignment(git, value.clone(), "hosts/server.rb", 4),
    ]);
    let config = schema.merge(&[], &host).unwrap();
    assert_eq!(config.options[&path(accounts)].value, json!(["research"]));
    assert_eq!(config.options[&path(git)].value, value);

    for (name, value, specifics) in [
        (
            "users.users.alice.extraGroup",
            json!(["research"]),
            "users.users.alice.extraGroup",
        ),
        (accounts, json!(["research", false]), "/1"),
    ] {
        let host = unit(vec![assignment(name, value, "hosts/server.rb", 9)]);
        let error = schema.merge(&[], &host).unwrap_err().to_string();
        for text in [name, "hosts/server.rb:9", specifics] {
            assert!(error.contains(text), "{error}");
        }
    }
}

#[test]
fn overlapping_option_definitions_report_both_modules_without_partial_registration() {
    let mut schema = registry(json!([{
        "path": "users.users.<name>.extraGroups", "type": {"list": "string"},
        "description": "Supplementary groups"
    }]));
    let replacement = json!({"options": [
        {"path": "test.other", "type": "string", "default": "added", "description": "Other"},
        {"path": "users.users.alice.extraGroups", "type": {"list": "string"}, "description": "Duplicate"}
    ]});
    let error = schema
        .register_json(
            "replacement",
            "modules/replacement/options.json",
            &replacement.to_string(),
        )
        .unwrap_err()
        .to_string();
    for text in [
        "test",
        "replacement",
        "/options/0",
        "/options/1",
        "modules/test/options.json",
        "modules/replacement/options.json",
        "users.users.<name>.extraGroups",
        "users.users.alice.extraGroups",
    ] {
        assert!(error.contains(text), "{error}");
    }
    assert!(
        schema
            .merge(&[], &UnitSpec::default())
            .unwrap()
            .options
            .is_empty()
    );
}

#[test]
fn relative_source_file_values_compare_in_their_declaring_directories() {
    let schema = registry(json!([{
        "path": "services.caddy.configFile", "type": "sourceFile",
        "description": "Caddy configuration file"
    }]));
    let name = "services.caddy.configFile";
    let first = assignment(name, json!("Caddyfile"), "profiles/one/profile.rb", 4);
    let sibling = assignment(name, json!("./Caddyfile"), "profiles/one/extra.rb", 5);
    let other = assignment(name, json!("Caddyfile"), "profiles/two/profile.rb", 6);
    let config = schema
        .merge(&[unit(vec![first.clone(), sibling])], &UnitSpec::default())
        .unwrap();
    let option = &config.options[&path(name)];
    assert_eq!(option.value, "Caddyfile");
    assert_eq!(option.effective_definitions[0].origin, first.origin);
    assert_eq!(option.effective_definitions.len(), 2);
    let error = schema
        .merge(&[unit(vec![first, other])], &UnitSpec::default())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("profiles/one/profile.rb:4") && error.contains("profiles/two/profile.rb:6"),
        "{error}"
    );
}

#[test]
fn rejects_invalid_defaults_before_accepting_any_option_from_the_module() {
    let mut schema = SchemaRegistry::default();
    let invalid = json!({"options": [
        {"path": "test.first", "type": "any", "default": null, "description": "Allows null"},
        {"path": "test.invalid", "type": {"map": {"list": "integer"}},
         "default": {"a/b~c": [1, "wrong"]}, "description": "Nested integers"}
    ]});
    let error = schema
        .register_json("test", "options.json", &invalid.to_string())
        .unwrap_err()
        .to_string();
    for text in [
        "options.json",
        "test.invalid",
        "/options/1",
        "/a~1b~0c/1",
        "integer",
    ] {
        assert!(error.contains(text), "{error}");
    }
    assert!(
        schema
            .merge(&[], &UnitSpec::default())
            .unwrap()
            .options
            .is_empty()
    );

    let valid = json!({"options": [invalid["options"][0].clone()]});
    schema
        .register_json("test", "options.json", &valid.to_string())
        .unwrap();
    let config = schema.merge(&[], &UnitSpec::default()).unwrap();
    assert!(config.options[&path("test.first")].value.is_null());
}

#[test]
fn defaults_for_named_options_apply_only_to_instantiated_paths() {
    let schema = registry(json!([{
        "path": "users.users.<name>.extraGroups", "type": {"list": "string"},
        "merge": "set", "default": ["base"], "description": "Supplementary groups"
    }]));
    assert!(
        schema
            .merge(&[], &UnitSpec::default())
            .unwrap()
            .options
            .is_empty()
    );

    let name = "users.users.alice.extraGroups";
    let host = unit(vec![assignment(
        name,
        json!(["research"]),
        "hosts/server.rb",
        3,
    )]);
    let config = schema.merge(&[], &host).unwrap();
    let option = &config.options[&path(name)];
    assert_eq!(option.value, json!(["base", "research"]));
    assert_eq!(
        option.effective_definitions[0].priority,
        Priority::ModuleDefault
    );
    assert_eq!(config.options.len(), 1);
}

#[test]
fn file_sets_deduplicate_by_declaring_directory_and_keep_original_definitions() {
    let schema = registry(json!([{
        "path": "test.files", "type": {"list": "sourceFile"}, "merge": "set",
        "description": "Input files"
    }]));
    let first = assignment(
        "test.files",
        json!(["./file"]),
        "profiles/first/config.rb",
        3,
    );
    let duplicate = assignment("test.files", json!(["file"]), "profiles/first/extra.rb", 4);
    let other = assignment("test.files", json!(["file"]), "profiles/other/config.rb", 8);
    let config = schema
        .merge(
            &[unit(vec![first.clone(), duplicate.clone(), other.clone()])],
            &UnitSpec::default(),
        )
        .unwrap();
    let option = &config.options[&path("test.files")];
    assert_eq!(option.value, json!(["./file", "file"]));
    assert_eq!(
        option
            .effective_definitions
            .iter()
            .map(|definition| &definition.origin)
            .collect::<Vec<_>>(),
        [&first.origin, &duplicate.origin, &other.origin],
    );
}
