use std::{env, fs, path::Path, process::Command};

use dek_controller::{
    eval::{CollectError, CollectRequest, Collector, JsonCollector, collect_graph},
    repo::{HostSelector, InputPolicy, Repository, Target},
};
use dek_core::{InputKind, Observation, Origin, Priority, ProfileRef, UnitSpec};
use serde_json::{Value, json};

fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

const EMPTY: &str = r#"{"schemaVersion":1,"imports":[],"options":[]}"#;
const MANIFEST: &str = r#"{
    "schemaVersion":1,
    "hosts":{
        "laptop": {"entry":"hosts/laptop.json","target":{"local":true}},
        "server": {"entry":"hosts/server.json","target":{"ssh":"lab-server"}}
    },
    "profiles":{
        "base":"profiles/base.json",
        "extra":{"entry":"profiles/extra.json","inputs":"sdk"}
    }
}"#;

fn repo(root: &Path, host: &str, base: &str, extra: &str) -> Repository {
    write(root, "dek.json", MANIFEST);
    write(root, "hosts/laptop.json", host);
    write(root, "profiles/base.json", base);
    write(root, "profiles/extra.json", extra);
    Repository::load(root).unwrap()
}

fn json_origin(file: &str, pointer: &str) -> Origin {
    Origin::Json {
        file: file.into(),
        pointer: pointer.parse().unwrap(),
    }
}

#[test]
fn collects_only_selected_host_and_its_reachable_profiles() {
    let directory = tempfile::tempdir().unwrap();
    let repo = repo(
        directory.path(),
        r#"{"schemaVersion":1,"imports":["base"],"options":[{"path":"programs.git.enable","value":false}]}"#,
        r#"{"schemaVersion":1,"imports":["extra"],"options":[{"path":"programs.git.enable","value":true}]}"#,
        EMPTY,
    );
    write(directory.path(), "hosts/server.json", "broken JSON");

    let graph = collect_graph(&repo, HostSelector::Named("laptop"), &mut JsonCollector).unwrap();
    assert_eq!(graph.host.name, "laptop");
    assert_eq!(graph.host.entry, "hosts/laptop.json");
    assert_eq!(graph.host.priority, Priority::Host);
    assert_eq!(graph.host.dependencies, [1]);
    assert_eq!(graph.profiles.len(), 2);
    assert_eq!(graph.profiles[0].name, "extra");
    assert_eq!(graph.profiles[0].input_policy, Some(InputPolicy::Sdk));
    assert_eq!(graph.profiles[1].name, "base");
    assert_eq!(graph.profiles[1].priority, Priority::Profile);
    assert_eq!(graph.profiles[1].dependencies, [0]);
    assert_eq!(
        graph.host.spec.imports[0].origin,
        Some(json_origin("hosts/laptop.json", "/imports/0"))
    );
    let assignment = &graph.profiles[1].spec.options[0];
    assert_eq!(assignment.path.as_str(), "programs.git.enable");
    assert_eq!(assignment.value, true);
    assert_eq!(
        assignment.origin,
        json_origin("profiles/base.json", "/options/0")
    );
    let inputs = &graph.profiles[0].spec.inputs;
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0].kind, InputKind::File);
    assert_eq!(inputs[0].id, "profiles/extra.json");
    assert_eq!(
        inputs[0].observation,
        Observation::Digest(
            "sha256:c9f2c174f6347ba4f64e2b377cd87cf3fafdc003226c0e952124ef8bb0795e67".into()
        )
    );
}

struct RecordingCollector(Vec<(String, Value)>);

impl Collector for RecordingCollector {
    fn collect(&mut self, request: &CollectRequest<'_>) -> Result<UnitSpec, CollectError> {
        assert_eq!(request.host, "server");
        assert_eq!(request.target, &Target::Ssh("lab-server".into()));
        assert_eq!(request.entry_path(), request.repo_root.join(request.entry));
        self.0.push((request.name.into(), json!(request.params)));
        JsonCollector.collect(request)
    }
}

#[test]
fn profile_identity_normalizes_nested_object_keys_and_preserves_array_order() {
    let directory = tempfile::tempdir().unwrap();
    let repo = repo(directory.path(), EMPTY, EMPTY, EMPTY);
    write(
        directory.path(),
        "hosts/server.json",
        r#"{
            "schemaVersion":1, "options":[],
            "imports":[
                {"name":"base","params":{"obj":{"a":0,"b":["x","y"]}}},
                {"name":"base","params":{"obj":{"b":["x","y"],"a":0}}},
                {"name":"base","params":{"obj":{"a":0,"b":["y","x"]}}}
            ]
        }"#,
    );
    let mut collected = RecordingCollector(Vec::new());
    let graph = collect_graph(&repo, HostSelector::Named("server"), &mut collected).unwrap();
    assert_eq!(
        collected.0,
        [
            ("server".into(), json!({})),
            ("base".into(), json!({"obj":{"a":0,"b":["x","y"]}})),
            ("base".into(), json!({"obj":{"a":0,"b":["y","x"]}})),
        ]
    );
    assert_eq!(graph.host.dependencies, [0, 0, 1]);
    assert_eq!(graph.profiles.len(), 2);
    assert_eq!(
        graph.host.spec.imports[1].origin,
        Some(json_origin("hosts/server.json", "/imports/1"))
    );
}

#[test]
fn profile_failures_report_the_import_chain_and_reference_location() {
    let directory = tempfile::tempdir().unwrap();
    let repo = repo(
        directory.path(),
        r#"{"schemaVersion":1,"imports":["base"],"options":[]}"#,
        r#"{"schemaVersion":1,"imports":["extra"],"options":[]}"#,
        r#"{"schemaVersion":1,"imports":["base"],"options":[]}"#,
    );

    let error = collect_graph(&repo, HostSelector::Named("laptop"), &mut JsonCollector)
        .unwrap_err()
        .to_string();
    for text in [
        "循环",
        "laptop",
        "base",
        "extra",
        "profiles/extra.json",
        "/imports/0",
    ] {
        assert!(error.contains(text), "{error}");
    }
    assert_eq!(error.matches("/imports/0").count(), 3, "{error}");

    write(
        directory.path(),
        "profiles/extra.json",
        r#"{"schemaVersion":1,"imports":["unknown"],"options":[]}"#,
    );
    let error = collect_graph(&repo, HostSelector::Named("laptop"), &mut JsonCollector)
        .unwrap_err()
        .to_string();
    for text in [
        "unknown",
        "laptop",
        "base",
        "extra",
        "profiles/extra.json",
        "/imports/0",
    ] {
        assert!(error.contains(text), "{error}");
    }
}

#[test]
fn invalid_static_units_report_the_file_and_field() {
    let directory = tempfile::tempdir().unwrap();
    let repo = repo(directory.path(), EMPTY, EMPTY, EMPTY);
    for (fixture, pointer, reason) in [
        (
            r#"{"schemaVersion":2,"newField":true}"#,
            "/schemaVersion",
            "2",
        ),
        (
            r#"{"schemaVersion":1,"imports":[],"options":[{"path":false,"value":true}]}"#,
            "/options/0/path",
            "boolean",
        ),
        (
            r#"{"schemaVersion":1,"imports":[{"name":"base","params":[]}],"options":[]}"#,
            "/imports/0/params",
            "sequence",
        ),
        (
            r#"{"schemaVersion":1,"imports":[],"options":[["programs.git.enable",true]]}"#,
            "/options/0",
            "object",
        ),
        (
            r#"{"schemaVersion":1,"imports":[],"options":[{"path":"programs.git.enable"}]}"#,
            "/options/0",
            "value",
        ),
    ] {
        write(directory.path(), "hosts/laptop.json", fixture);
        let error = collect_graph(&repo, HostSelector::Named("laptop"), &mut JsonCollector)
            .unwrap_err()
            .to_string();
        assert!(
            ["hosts/laptop.json", pointer, reason]
                .into_iter()
                .all(|text| error.contains(text)),
            "{error}"
        );
    }
}

#[test]
fn static_graph_evaluates_without_ruby_on_path() {
    const CHILD: &str = "DEK_JSON_GRAPH_TEST_NO_RUBY";
    let directory = tempfile::tempdir().unwrap();
    if env::var_os(CHILD).is_none() {
        let output = Command::new(env::current_exe().unwrap())
            .args(["--exact", "static_graph_evaluates_without_ruby_on_path"])
            .env(CHILD, "1")
            .env("PATH", directory.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let repo = repo(directory.path(), EMPTY, EMPTY, EMPTY);
    let graph = collect_graph(&repo, HostSelector::Named("laptop"), &mut JsonCollector).unwrap();
    assert_eq!(graph.host.entry, "hosts/laptop.json");
}

#[test]
fn permits_an_injected_collector_to_import_a_static_profile() {
    struct MixedCollector;

    impl Collector for MixedCollector {
        fn collect(&mut self, request: &CollectRequest<'_>) -> Result<UnitSpec, CollectError> {
            if request.entry == "hosts/laptop.rb" {
                assert_eq!(request.priority, Priority::Host);
                assert_eq!(request.input_policy, Some(InputPolicy::Sdk));
                return Ok(UnitSpec {
                    imports: vec![ProfileRef {
                        name: "base".into(),
                        params: Default::default(),
                        origin: Some(Origin::Ruby {
                            file: request.entry.into(),
                            line: 1.try_into().unwrap(),
                        }),
                    }],
                    ..UnitSpec::default()
                });
            }
            JsonCollector.collect(request)
        }
    }

    let directory = tempfile::tempdir().unwrap();
    write(
        directory.path(),
        "dek.json",
        r#"{
            "schemaVersion":1,
            "hosts":{"laptop":{"entry":"./hosts/laptop.rb","inputs":"sdk","target":{"local":true}}},
            "profiles":{"base":"profiles/./base.json"}
        }"#,
    );
    write(directory.path(), "hosts/laptop.rb", "profile \"base\"\n");
    write(directory.path(), "profiles/base.json", EMPTY);

    let repo = Repository::load(directory.path()).unwrap();
    let graph = collect_graph(&repo, HostSelector::Local("laptop"), &mut MixedCollector).unwrap();
    assert_eq!(graph.host.entry, "hosts/laptop.rb");
    assert_eq!(graph.host.dependencies, [0]);
    assert_eq!(
        graph.host.spec.imports[0]
            .origin
            .as_ref()
            .unwrap()
            .to_string(),
        "hosts/laptop.rb:1"
    );
    assert_eq!(graph.profiles[0].entry, "profiles/base.json");
    assert_eq!(graph.profiles[0].spec.inputs[0].id, "profiles/base.json");
}
