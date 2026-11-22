use dek_core::{
    HostConfig, InputKind, JsonPointer, Observation, OptionPath, Origin, Priority, ProfileRef,
    UnitSpec,
};
use serde_json::{Value, json};

const UNIT_SPEC: &str = include_str!("fixtures/unit-spec.json");
const HOST_CONFIG: &str = include_str!("fixtures/host-config.json");

#[test]
fn unit_spec_round_trips_assignments_parameters_and_tracked_inputs() {
    let unit: UnitSpec = serde_json::from_str(UNIT_SPEC).unwrap();
    let encoded = serde_json::to_string(&unit).unwrap();
    let decoded: UnitSpec = serde_json::from_str(&encoded).unwrap();

    assert_eq!(decoded, unit);
    assert_eq!(
        serde_json::from_str::<Value>(&encoded).unwrap(),
        serde_json::from_str::<Value>(UNIT_SPEC).unwrap()
    );
    assert_eq!(decoded.imports[0].name, "base");
    assert_eq!(
        decoded.imports[0].params["backup"],
        json!({
            "paths": ["/srv/projects", "/srv/data"],
            "schedule": { "days": ["Mon", "Fri"], "hour": 3 }
        })
    );
    assert_eq!(decoded.imports[0].params["label"], Value::Null);
    assert!(matches!(
        decoded.imports[0].origin,
        Some(Origin::Ruby { line, .. }) if line.get() == 2
    ));
    assert_eq!(decoded.options.len(), 2);
    assert_eq!(decoded.options[0].path.as_str(), "programs.git.config");
    assert_eq!(decoded.options[0].path, decoded.options[1].path);
    assert!(matches!(
        &decoded.options[0].origin,
        Origin::Json { file, pointer }
            if file == "profiles/base.json" && pointer.as_str() == "/options/0"
    ));
    assert!(matches!(
        &decoded.options[1].origin,
        Origin::Ruby { file, line } if file == "hosts/server/default.rb" && line.get() == 5
    ));
    assert_eq!(decoded.inputs.len(), 2);
    assert_eq!(decoded.inputs[0].kind, InputKind::File);
    assert!(matches!(
        &decoded.inputs[0].observation,
        Observation::Digest(digest) if digest.starts_with("sha256:")
    ));
    assert_eq!(decoded.inputs[1].kind, InputKind::TargetFile);
    assert_eq!(decoded.inputs[1].id, "/etc/dek/override");
    assert_eq!(decoded.inputs[1].observation, Observation::Missing);
    assert!(matches!(
        decoded.inputs[1].origin,
        Some(Origin::Ruby { line, .. }) if line.get() == 8
    ));
}

#[test]
fn host_config_round_trips_effective_and_overridden_definitions() {
    let unit: UnitSpec = serde_json::from_str(UNIT_SPEC).unwrap();
    let config: HostConfig = serde_json::from_str(HOST_CONFIG).unwrap();
    let encoded = serde_json::to_value(&config).unwrap();
    let decoded: HostConfig = serde_json::from_value(encoded.clone()).unwrap();

    assert_eq!(decoded, config);
    assert_eq!(encoded, serde_json::from_str::<Value>(HOST_CONFIG).unwrap());
    let option = &decoded.options[&unit.options[0].path];
    assert_eq!(option.value, unit.options[1].value);
    assert_eq!(option.priority, Priority::Host);
    assert_eq!(option.effective_definitions.len(), 1);
    let effective = &option.effective_definitions[0];
    assert_eq!(effective.value, unit.options[1].value);
    assert_eq!(effective.origin, unit.options[1].origin);
    assert_eq!(effective.priority, Priority::Host);
    assert_eq!(option.overridden_definitions.len(), 1);
    let overridden = &option.overridden_definitions[0];
    assert_eq!(overridden.value, unit.options[0].value);
    assert_eq!(overridden.origin, unit.options[0].origin);
    assert_eq!(overridden.priority, Priority::Profile);
    assert!(Priority::ModuleDefault < Priority::Profile && Priority::Profile < Priority::Host);
}

#[test]
fn protocol_example_accepts_a_profile_name_without_parameters() {
    let unit: UnitSpec = serde_json::from_value(json!({
        "imports": ["base"],
        "options": [{
            "path": "services.caddy.enable",
            "value": true,
            "origin": {"file": "hosts/server/default.rb", "line": 5}
        }],
        "inputs": []
    }))
    .unwrap();

    assert_eq!(
        unit.imports,
        [ProfileRef {
            name: "base".into(),
            params: Default::default(),
            origin: None,
        }]
    );
    assert_eq!(
        serde_json::from_str::<UnitSpec>(&serde_json::to_string(&unit).unwrap()).unwrap(),
        unit
    );
}

#[test]
fn option_paths_preserve_case_and_reject_empty_components() {
    let path: OptionPath = "users.users.alice.extraGroups".parse().unwrap();
    assert_eq!(path.as_str(), "users.users.alice.extraGroups");
    assert_eq!(
        serde_json::to_value(path).unwrap(),
        "users.users.alice.extraGroups"
    );

    for path in ["", ".services", "programs..git", "services.", "a.\nb"] {
        assert!(path.parse::<OptionPath>().is_err(), "{path:?}");
        assert!(
            serde_json::from_value::<OptionPath>(json!(path)).is_err(),
            "{path:?}"
        );
    }

    let mut config: Value = serde_json::from_str(HOST_CONFIG).unwrap();
    config["options"]["programs..git"] = config["options"]["programs.git.config"].clone();
    assert!(serde_json::from_value::<HostConfig>(config).is_err());
}

#[test]
fn origin_requires_one_valid_location() {
    for source in [
        json!({"file": "source"}),
        json!({"file": "source", "line": 0}),
        json!({"file": "source", "line": 5, "pointer": "/options/0"}),
        json!({"file": "source", "pointer": "/options/~2"}),
        json!({"file": "source", "pointer": "options/0"}),
    ] {
        assert!(
            serde_json::from_value::<Origin>(source.clone()).is_err(),
            "{source}"
        );
    }

    for pointer in ["", "/", "/a~1b/~0name/~01"] {
        let origin: Origin =
            serde_json::from_value(json!({"file": "config.json", "pointer": pointer})).unwrap();
        assert_eq!(
            origin,
            Origin::Json {
                file: "config.json".into(),
                pointer: pointer.parse::<JsonPointer>().unwrap()
            }
        );
    }
}

#[test]
fn missing_input_is_explicit_and_distinct_from_an_observed_null_value() {
    let mut unit: Value = serde_json::from_str(UNIT_SPEC).unwrap();
    assert!(serde_json::from_value::<UnitSpec>(unit.clone()).is_ok());
    unit["inputs"][1]["observation"] = Value::Null;
    assert!(serde_json::from_value::<UnitSpec>(unit.clone()).is_err());
    unit["inputs"][1]
        .as_object_mut()
        .unwrap()
        .remove("observation");
    assert!(serde_json::from_value::<UnitSpec>(unit).is_err());
}

#[test]
fn models_round_trip_with_positional_messagepack_fields() {
    let unit: UnitSpec = serde_json::from_str(UNIT_SPEC).unwrap();
    let config: HostConfig = serde_json::from_str(HOST_CONFIG).unwrap();
    let unit_bytes = rmp_serde::to_vec(&unit).unwrap();
    let config_bytes = rmp_serde::to_vec(&config).unwrap();

    assert_eq!(
        rmp_serde::from_slice::<UnitSpec>(&unit_bytes).unwrap(),
        unit
    );
    assert_eq!(
        rmp_serde::from_slice::<HostConfig>(&config_bytes).unwrap(),
        config
    );
}

#[test]
fn profile_reference_rejects_misspelled_parameter_fields() {
    let reference = json!({
        "name": "base",
        "parameters": {"enabled": true}
    });

    assert!(serde_json::from_value::<ProfileRef>(reference).is_err());
}

#[test]
fn displays_the_file_and_original_source_location() {
    let unit: UnitSpec = serde_json::from_str(UNIT_SPEC).unwrap();

    assert_eq!(
        unit.options[0].origin.to_string(),
        "profiles/base.json (JSON Pointer \"/options/0\")"
    );
    assert_eq!(
        unit.options[1].origin.to_string(),
        "hosts/server/default.rb:5"
    );
}
