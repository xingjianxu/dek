use std::{fs, path::Path};

use dek_controller::{
    eval::{
        HttpInputProvider, JsonCollector, LanguageCollector, RubyProviders, RubySession,
        TargetAnswer, TargetProvider, collect_graph,
    },
    repo::{HostSelector, InputPolicy, Repository},
};
use dek_core::{
    InputKind, Observation, Origin,
    protocol::{EvalOperation, TargetQuery, TargetQueryKind},
};
use serde_json::{Value, json};

fn write(root: &Path, path: &str, contents: &str) {
    let file = root.join(path);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(file, contents).unwrap();
}

fn repository(root: &Path, manifest: Value) -> Repository {
    write(root, "dek.json", &manifest.to_string());
    Repository::load(root).unwrap()
}

#[derive(Default)]
struct Target(Vec<TargetQuery>);

impl TargetProvider for Target {
    fn query(&mut self, query: &TargetQuery) -> Result<TargetAnswer, String> {
        self.0.push(query.clone());
        let (value, observation) = match (query.kind, query.key.as_str()) {
            (TargetQueryKind::Fact, "architecture") => (
                json!("aarch64"),
                Observation::Digest("target-architecture".into()),
            ),
            (TargetQueryKind::Capability, "systemd") => {
                (json!(true), Observation::Digest("target-systemd".into()))
            }
            _ => (Value::Null, Observation::Missing),
        };
        Ok(TargetAnswer { value, observation })
    }
}

#[test]
fn ruby_shorthands_produce_the_same_assignments_as_json() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let repo = repository(
        root,
        json!({
            "schemaVersion":1,
            "hosts":{
                "ruby":{"entry":"hosts/default.rb","target":{"local":true}},
                "json":{"entry":"hosts/default.json","target":{"local":true}}
            }
        }),
    );
    write(
        root,
        "hosts/default.rb",
        r#"service :caddy, enable: true, configFile: source_file("./Caddyfile")
group "research"
group "ops", gid: 1010
user "alice", isNormalUser: true, shell: "/bin/bash", extraGroups: ["research"]
directory "/srv/research.data", owner: "alice", group: "research", mode: "0750"
configure "lab.backup", enable: true, directories: ["/srv"]
"#,
    );
    write(root, "hosts/default.json", &json!({
        "schemaVersion":1, "imports":[],
        "options":[
            {"path":"services.caddy.enable","value":true},
            {"path":"services.caddy.configFile","value":"./Caddyfile"},
            {"path":"users.groups.research","value":{}},
            {"path":"users.groups.ops.gid","value":1010},
            {"path":"users.users.alice.isNormalUser","value":true},
            {"path":"users.users.alice.shell","value":"/bin/bash"},
            {"path":"users.users.alice.extraGroups","value":["research"]},
            {"path":"systemd.tmpfiles.settings.dek","value":{"/srv/research.data":{"d":{"user":"alice","group":"research","mode":"0750"}}}},
            {"path":"lab.backup.enable","value":true},
            {"path":"lab.backup.directories","value":["/srv"]}
        ]
    }).to_string());
    let ruby = collect_graph(
        &repo,
        HostSelector::Named("ruby"),
        &mut LanguageCollector::new(&mut RubySession::new(), &mut Target::default()),
    )
    .unwrap();
    let json = collect_graph(&repo, HostSelector::Named("json"), &mut JsonCollector).unwrap();
    let ruby_options = &ruby.host.spec.options;
    assert_eq!(ruby_options.len(), json.host.spec.options.len());
    for (i, (ruby, json)) in ruby_options.iter().zip(&json.host.spec.options).enumerate() {
        assert_eq!(ruby.path, json.path);
        assert_eq!(ruby.value, json.value);
        let line = [1, 1, 2, 3, 4, 4, 4, 5, 6, 6][i];
        assert!(matches!(&ruby.origin,
            Origin::Ruby { file, line: observed } if file == "hosts/default.rb" && observed.get() == line
        ));
    }
    assert_eq!(ruby.host.spec.inputs.len(), 1);
    assert_eq!(ruby.host.spec.inputs[0].id, "hosts/default.rb");
}

#[test]
fn expands_mixed_language_profiles_and_reattributes_loaded_helpers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let repo = repository(
        root,
        json!({
            "schemaVersion":1,
            "hosts":{"server":{"entry":"hosts/server.rb","inputs":"sdk","target":{"ssh":"lab-server"}}},
            "profiles":{"bridge":"profiles/bridge.json","leaf":"profiles/leaf.rb"}
        }),
    );
    write(
        root,
        "hosts/server.rb",
        "require_relative '../lib/helper'\nprofile 'bridge'\n",
    );
    write(root, "lib/helper.rb", "require_relative 'nested'\n");
    write(
        root,
        "lib/nested.rb",
        "module Helper; LABEL = 'first'; end\n",
    );
    write(
        root,
        "profiles/leaf.rb",
        r#"$LOAD_PATH.unshift(File.expand_path("../lib", __dir__))
begin
  Kernel.require "helper"
ensure
  $LOAD_PATH.shift
end
configure 'example.label', Helper::LABEL + params.fetch('suffix')
configure 'example.native', File.read(File.join(__dir__, 'native.txt'))
"#,
    );
    write(root, "profiles/native.txt", "native Ruby input");
    write(
        root,
        "profiles/bridge.json",
        r#"{"schemaVersion":1,"imports":[{"name":"leaf","params":{"suffix":"!"}}],"options":[]}"#,
    );
    let evaluate = || {
        collect_graph(
            &repo,
            HostSelector::Named("server"),
            &mut LanguageCollector::new(&mut RubySession::new(), &mut Target::default()),
        )
        .unwrap()
    };
    let graph = evaluate();
    let [leaf, bridge] = graph.profiles.as_slice() else {
        panic!("expected the leaf and bridge")
    };
    assert_eq!(graph.host.dependencies, [1]);
    assert_eq!(bridge.dependencies, [0]);
    assert_eq!(graph.host.input_policy, Some(InputPolicy::Sdk));
    assert_eq!(leaf.input_policy, None);
    assert_eq!(
        graph.host.spec.imports[0]
            .origin
            .as_ref()
            .unwrap()
            .to_string(),
        "hosts/server.rb:2"
    );
    assert_eq!(
        bridge.spec.imports[0].origin.as_ref().unwrap().to_string(),
        "profiles/bridge.json (JSON Pointer \"/imports/0\")"
    );
    assert_eq!(leaf.spec.options[0].value, "first!");
    assert_eq!(leaf.spec.options[1].value, "native Ruby input");
    assert!(
        !leaf
            .spec
            .inputs
            .iter()
            .any(|input| input.id == "profiles/native.txt")
    );
    fn nested(spec: &dek_core::UnitSpec) -> &Observation {
        &spec
            .inputs
            .iter()
            .find(|input| input.id == "lib/nested.rb")
            .unwrap()
            .observation
    }
    assert_eq!(nested(&graph.host.spec), nested(&leaf.spec));
    assert!(
        leaf.spec
            .inputs
            .iter()
            .any(|input| input.id == "lib/helper.rb")
    );
    write(
        root,
        "lib/nested.rb",
        "module Helper; LABEL = 'second'; end\n",
    );
    let updated = evaluate();
    assert_ne!(nested(&leaf.spec), nested(&updated.profiles[0].spec));
    assert_eq!(updated.profiles[0].spec.options[0].value, "second!");
}

#[test]
fn records_only_consumed_inputs_and_distinguishes_missing_values() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let repo = repository(
        root,
        json!({
            "schemaVersion":1,
            "hosts":{"server":{"entry":"hosts/server.rb","inputs":"sdk","target":{"ssh":"lab-server"}}}
        }),
    );
    write(
        root,
        "hosts/server.rb",
        r#"service :caddy, configFile: source_file("Caddyfile")
configure "example.file", read_text("read.txt")
configure "example.directory", read_dir("members")
configure "example.environment", env("PATH")
configure "example.localMissing", read_text("missing.txt")
configure "example.targetMissing", target.read_text("/etc/dek/optional")
configure "example.architecture", target.fact("architecture")
configure "example.systemd", target.capability("systemd")
configure "example.deployed", source_file("read.txt")
"#,
    );
    write(root, "hosts/Caddyfile", "static config");
    write(root, "hosts/read.txt", "hello\n");
    write(root, "hosts/members/zeta", "last");
    write(root, "hosts/members/alpha", "first");
    let graph = collect_graph(
        &repo,
        HostSelector::Named("server"),
        &mut LanguageCollector::new(&mut RubySession::new(), &mut Target::default()),
    )
    .unwrap();
    let spec = graph.host.spec;
    assert_eq!(spec.options[0].value, "Caddyfile");
    assert_eq!(spec.options[1].value, "hello\n");
    assert_eq!(spec.options[2].value, json!(["alpha", "zeta"]));
    assert_eq!(spec.options[4].value, Value::Null);
    assert_eq!(spec.options[5].value, Value::Null);
    assert_eq!(spec.options[6].value, "aarch64");
    assert_eq!(spec.options[7].value, true);
    assert_eq!(spec.options[8].value, "read.txt");
    assert!(
        !spec
            .inputs
            .iter()
            .any(|input| input.id.ends_with("Caddyfile"))
    );
    let file = spec
        .inputs
        .iter()
        .find(|input| input.id == "hosts/read.txt")
        .unwrap();
    assert_eq!(
        file.observation,
        Observation::Digest(
            "sha256:5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03".into()
        )
    );
    assert_eq!(
        file.origin.as_ref().unwrap().to_string(),
        "hosts/server.rb:2"
    );
    for (kind, id, missing) in [
        (InputKind::Directory, "hosts/members", false),
        (InputKind::Environment, "PATH", false),
        (InputKind::File, "hosts/missing.txt", true),
        (InputKind::TargetFile, "/etc/dek/optional", true),
        (InputKind::TargetFact, "architecture", false),
        (InputKind::TargetCapability, "systemd", false),
    ] {
        let input = spec
            .inputs
            .iter()
            .find(|input| input.kind == kind && input.id == id)
            .unwrap();
        assert_eq!(input.observation == Observation::Missing, missing);
        assert!(input.origin.as_ref().unwrap().file() == "hosts/server.rb");
    }
}

#[test]
fn absent_target_fact_fails_at_the_ruby_call() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let repo = repository(
        root,
        json!({
            "schemaVersion":1,"hosts":{"server":{"entry":"server.rb","target":{"ssh":"lab-server"}}}
        }),
    );
    write(
        root,
        "server.rb",
        "\nconfigure 'example.fact', target.fact('unknown')\n",
    );
    let error = collect_graph(
        &repo,
        HostSelector::Named("server"),
        &mut LanguageCollector::new(&mut RubySession::new(), &mut Target::default()),
    )
    .unwrap_err()
    .to_string();
    for expected in ["server.rb:2", "unknown", "fact"] {
        assert!(error.contains(expected), "{error}");
    }
}

struct Http(Result<Option<String>, String>);

impl HttpInputProvider for Http {
    fn read(&mut self, url: &str) -> Result<Option<String>, String> {
        assert_eq!(url, "https://example.test/settings");
        self.0.clone()
    }
}

#[test]
fn http_adapter_distinguishes_empty_response_missing_value_and_error() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let repo = repository(
        root,
        json!({
            "schemaVersion":1,"hosts":{"server":{"entry":"server.rb","target":{"ssh":"lab-server"}}}
        }),
    );
    write(
        root,
        "server.rb",
        "\nconfigure 'example.response', http_text('https://example.test/settings')\n",
    );
    let evaluate = |reply| {
        collect_graph(
            &repo,
            HostSelector::Named("server"),
            &mut LanguageCollector::new(&mut RubySession::new(), &mut Target::default())
                .with_http_provider(&mut Http(reply)),
        )
    };
    let graph = evaluate(Ok(Some(String::new()))).unwrap();
    assert_eq!(graph.host.spec.options[0].value, "");
    let input = graph
        .host
        .spec
        .inputs
        .iter()
        .find(|input| input.kind == InputKind::Http)
        .unwrap();
    assert_eq!(input.id, "https://example.test/settings");
    assert_eq!(
        input.observation,
        Observation::Digest(
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into()
        )
    );
    for (response, reason) in [
        (Ok(None), "refresh"),
        (Err("response failed".into()), "response failed"),
    ] {
        let error = evaluate(response).unwrap_err().to_string();
        for expected in ["server.rb:2", "https://example.test/settings", reason] {
            assert!(error.contains(expected), "{error}");
        }
    }
}

#[test]
fn sdk_expansion_retains_its_result_and_records_loaded_code() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "modules/helper.rb",
        "module ModuleHelper; LABEL = 'expand'; end",
    );
    write(root, "modules/template.txt", "contents");
    write(
        root,
        "modules/expand.rb",
        r#"require_relative "helper"
{ label: ModuleHelper::LABEL, suffix: context.fetch("suffix"), input: read_text("template.txt") }"#,
    );
    let output = RubySession::new()
        .evaluate_with_sdk(
            EvalOperation::Expand,
            root,
            &root.join("modules/expand.rb"),
            json!({"suffix": "!"}),
            RubyProviders {
                target: &mut Target::default(),
                http: None,
            },
        )
        .unwrap();
    assert_eq!(
        output.result,
        json!({"label":"expand","suffix":"!","input":"contents"})
    );
    assert_eq!(
        output
            .inputs
            .iter()
            .map(|input| input.id.as_str())
            .collect::<Vec<_>>(),
        [
            "modules/expand.rb",
            "modules/helper.rb",
            "modules/template.txt"
        ]
    );
}

#[test]
fn refuses_to_cache_source_that_changes_while_ruby_loads_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let repo = repository(
        root,
        json!({
            "schemaVersion": 1,
            "hosts":{"server":{"entry":"server.rb","inputs":"sdk","target":{"local":true}}}
        }),
    );
    write(
        root,
        "server.rb",
        "require_relative 'helper'\nconfigure 'example.ready', true\n",
    );
    write(
        root,
        "helper.rb",
        r##"File.write(__FILE__, "# changed while loading\n")"##,
    );
    let error = collect_graph(
        &repo,
        HostSelector::Named("server"),
        &mut LanguageCollector::new(&mut RubySession::new(), &mut Target::default()),
    )
    .unwrap_err()
    .to_string();
    for expected in ["server.rb:1", "helper.rb", "changed during evaluation"] {
        assert!(error.contains(expected), "{error}");
    }
}
