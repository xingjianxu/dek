use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    fs,
    path::Path,
    process::Command,
};

use dek_cli::{plan_output::write_plan, planning::plan_host};
use dek_controller::{
    eval::{TargetAnswer, TargetProvider},
    repo::{HostSelector, Repository},
};
use dek_core::{
    ActionKind, ContentDigest, DesiredResource, FileAttributes, FileContent, FileObservation,
    FileState, HostConfig, Precondition, ResourceCapability, ResourceKind, ServiceNotification,
    TargetIdentity, protocol::TargetQuery,
};
use dek_target::backend::{
    Backend, BackendError, BackendRegistry, PlannedAction, PlannedNotification,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[cfg(unix)]
#[path = "planning/incremental.rs"]
mod incremental;

const CADDY_PATH: &str = "/etc/caddy/Caddyfile";
const CADDY_INITIAL: &str = ":8080 {\n  respond \"hello\"\n}\n";
const CADDY_UPDATED: &str = ":8080 {\n  respond \"updated\"\n}\n";

#[derive(Default)]
struct MemoryTarget {
    deployed: Cell<bool>,
    actual_files: RefCell<BTreeMap<String, FileContent>>,
    executes: Cell<usize>,
}

impl MemoryTarget {
    fn set_file(&self, path: &str, digest: ContentDigest) {
        self.set_file_content(
            path,
            FileContent {
                digest,
                attributes: FileAttributes::default(),
            },
        );
    }

    fn set_file_content(&self, path: &str, content: FileContent) {
        self.actual_files.borrow_mut().insert(path.into(), content);
    }

    fn registry(&self) -> BackendRegistry<'_> {
        let mut registry = BackendRegistry::new(
            TargetIdentity {
                id: "memory-server".into(),
                os: "linux".into(),
                distribution: Some("arch".into()),
                architecture: "x86_64".into(),
            },
            1,
            |path| Ok(path.to_owned()),
        );
        registry.register(self).unwrap();
        registry
    }
}

impl Backend for MemoryTarget {
    fn capabilities(&self) -> Vec<ResourceCapability> {
        [
            (ResourceKind::Package, &[][..]),
            (ResourceKind::File, &["mode"]),
            (ResourceKind::SystemdUnit, &["enabled", "running", "reload"]),
        ]
        .into_iter()
        .map(|(kind, attributes)| ResourceCapability {
            kind,
            version: 1,
            attributes: attributes
                .iter()
                .map(|attribute| (*attribute).into())
                .collect(),
        })
        .collect()
    }

    fn validate(&self, _: &DesiredResource) -> Result<(), BackendError> {
        Ok(())
    }

    fn observe(&self, resource: &DesiredResource) -> Result<Vec<Precondition>, BackendError> {
        let deployed = self.deployed.get();
        Ok(match resource {
            DesiredResource::Package(package) => vec![Precondition::PackageVersion {
                package: package.package.clone(),
                expected: deployed.then_some("2.0.0".into()),
            }],
            DesiredResource::File(file) => {
                let actual = self.actual_files.borrow().get(&file.path).cloned();
                vec![Precondition::File {
                    path: file.path.clone(),
                    expected: actual.map_or(FileObservation::Missing, FileObservation::Present),
                }]
            }
            DesiredResource::SystemdUnit(unit) => vec![
                Precondition::UnitEnabled {
                    unit: unit.unit.clone(),
                    expected: deployed,
                },
                Precondition::UnitRunning {
                    unit: unit.unit.clone(),
                    expected: deployed,
                },
            ],
            _ => panic!("unexpected resource {resource:?}"),
        })
    }

    fn plan(
        &self,
        resource: &DesiredResource,
        observed: &[Precondition],
    ) -> Result<Vec<PlannedAction>, BackendError> {
        let action = |kind, reason: &str, preconditions, content_changed| PlannedAction {
            kind,
            reason: reason.into(),
            preconditions,
            content_changed,
        };
        let result = match (resource, observed) {
            (
                DesiredResource::Package(package),
                [Precondition::PackageVersion { expected: None, .. }],
            ) => {
                vec![action(
                    ActionKind::InstallPackage {
                        package: package.package.clone(),
                        candidate_version: "2.0.0".into(),
                    },
                    "软件包尚未安装",
                    observed.to_vec(),
                    false,
                )]
            }
            (DesiredResource::File(file), [Precondition::File { expected, .. }]) => {
                let FileState::Present(content) = &file.state else {
                    panic!("expected a present file");
                };
                if matches!(expected, FileObservation::Present(actual) if actual == content) {
                    Vec::new()
                } else {
                    vec![action(
                        ActionKind::WriteFile {
                            path: file.path.clone(),
                            content: content.clone(),
                        },
                        "当前文件内容与声明不同",
                        observed.to_vec(),
                        true,
                    )]
                }
            }
            (DesiredResource::SystemdUnit(unit), [enabled, running]) if !self.deployed.get() => {
                vec![
                    action(
                        ActionKind::SetUnitEnabled {
                            unit: unit.unit.clone(),
                            enabled: true,
                        },
                        "服务未启用自启",
                        vec![enabled.clone()],
                        false,
                    ),
                    action(
                        ActionKind::SetUnitRunning {
                            unit: unit.unit.clone(),
                            running: true,
                        },
                        "服务尚未运行",
                        vec![running.clone()],
                        false,
                    ),
                ]
            }
            _ => Vec::new(),
        };
        Ok(result)
    }

    fn plan_notification(
        &self,
        notice: &ServiceNotification,
        will_start: bool,
    ) -> Result<Option<PlannedNotification>, BackendError> {
        Ok(
            (self.deployed.get() || will_start).then(|| PlannedNotification {
                reason: "配置文件内容变化后重载服务".into(),
                preconditions: vec![Precondition::UnitRunning {
                    unit: notice.unit.clone(),
                    expected: true,
                }],
            }),
        )
    }

    fn execute(&self, _: &ActionKind) -> Result<(), BackendError> {
        self.executes.set(self.executes.get() + 1);
        Ok(())
    }

    fn verify(&self, _: &ActionKind) -> Result<bool, BackendError> {
        panic!("plan evaluated an execution verification hook")
    }
}

struct NoTargetQueries;

impl TargetProvider for NoTargetQueries {
    fn query(&mut self, query: &TargetQuery) -> Result<TargetAnswer, String> {
        panic!("unexpected target query {query:?}")
    }
}

fn write(root: &Path, relative: &str, data: impl AsRef<[u8]>) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, data).unwrap();
}

fn write_json(root: &Path, entry: &str, imports: &[&str], options: Value) {
    write(
        root,
        entry,
        json!({"schemaVersion":1,"imports":imports,"options":options}).to_string(),
    );
}

fn caddy_repo(root: &Path) -> Repository {
    write(
        root,
        "dek.json",
        r#"{
        "schemaVersion":1,
        "hosts":{
            "server":{"entry":"hosts/server/default.json","target":{"local":true}},
            "server-ruby":{"entry":"hosts/server/default.rb","target":{"local":true}},
            "server-remote":{"entry":"hosts/server/default.rb","target":{"ssh":"lab-server"}}
        },
        "profiles":{"base":"profiles/base.json","other":"profiles/other.rb"}
    }"#,
    );
    write_json(
        root,
        "profiles/base.json",
        &[],
        json!([
            {"path":"services.caddy.enable","value":false}
        ]),
    );
    write_json(
        root,
        "hosts/server/default.json",
        &["base"],
        json!([
            {"path":"services.caddy.enable","value":true},
            {"path":"services.caddy.configFile","value":"./Caddyfile"}
        ]),
    );
    write(
        root,
        "hosts/server/default.rb",
        "profile 'base'\nservice :caddy, enable: true, configFile: source_file('./Caddyfile')\n",
    );
    write(root, "hosts/server/Caddyfile", CADDY_INITIAL);
    Repository::load(root).unwrap()
}

fn semantics(config: &HostConfig) -> BTreeMap<&str, &Value> {
    config
        .options
        .iter()
        .map(|(path, option)| (path.as_str(), &option.value))
        .collect()
}

fn digest(bytes: &[u8]) -> ContentDigest {
    ContentDigest(format!("sha256:{:x}", Sha256::digest(bytes)))
}

#[test]
fn json_and_ruby_caddy_produce_the_same_ordered_plan_on_one_memory_target() {
    let root = tempfile::tempdir().unwrap();
    let repo = caddy_repo(root.path());
    let memory = MemoryTarget::default();
    let registry = memory.registry();
    let mut target = NoTargetQueries;

    let mut json = plan_host(&repo, HostSelector::Named("server"), &registry, &mut target).unwrap();
    let mut ruby = plan_host(
        &repo,
        HostSelector::Named("server-ruby"),
        &registry,
        &mut target,
    )
    .unwrap();
    assert_eq!(semantics(&json.config), semantics(&ruby.config));
    let json_file = json
        .expanded
        .resources
        .resources
        .iter()
        .find(|r| matches!(r.desired, DesiredResource::File(_)))
        .unwrap();
    let ruby_file = ruby
        .expanded
        .resources
        .resources
        .iter()
        .find(|r| matches!(r.desired, DesiredResource::File(_)))
        .unwrap();
    assert!(
        json_file.origins[0]
            .origin
            .to_string()
            .contains("default.json (JSON Pointer \"/options/1\")")
    );
    assert!(
        ruby_file.origins[0]
            .origin
            .to_string()
            .contains("default.rb:2")
    );
    assert_eq!(json.expanded.contents, ruby.expanded.contents);
    for run in [&mut json, &mut ruby] {
        for resource in &mut run.expanded.resources.resources {
            resource.origins.clear();
        }
        for action in &mut run.plan.actions {
            action.origins.clear();
        }
    }
    assert_eq!(json.expanded.resources, ruby.expanded.resources);
    assert_eq!(json.plan.actions, ruby.plan.actions);
    assert_eq!(json.plan.used_capabilities, ruby.plan.used_capabilities);
    let [package, file, enable, start] = &json.plan.actions[..] else {
        panic!("expected package, file and unit actions");
    };
    assert!(
        matches!(&package.kind, ActionKind::InstallPackage { package, .. } if package.name == "caddy")
    );
    assert!(matches!(&file.kind, ActionKind::WriteFile { path, .. } if path == CADDY_PATH));
    assert!(
        matches!(&enable.kind, ActionKind::SetUnitEnabled { unit, enabled: true } if unit.name == "caddy.service")
    );
    assert!(matches!(
        start.kind,
        ActionKind::SetUnitRunning { running: true, .. }
    ));
    assert_eq!(file.depends_on, std::slice::from_ref(&package.id));
    assert_eq!(enable.depends_on, [package.id.clone(), file.id.clone()]);
    assert_eq!(memory.executes.get(), 0);
}

#[test]
fn changed_caddy_content_updates_then_notifies_while_converged_plan_is_empty() {
    let root = tempfile::tempdir().unwrap();
    let repo = caddy_repo(root.path());
    let memory = MemoryTarget::default();
    let registry = memory.registry();
    memory.deployed.set(true);
    memory.set_file(CADDY_PATH, digest(CADDY_INITIAL.as_bytes()));
    let selector = HostSelector::Named("server");
    let safe = plan_host(&repo, selector, &registry, &mut NoTargetQueries).unwrap();
    assert!(safe.plan.actions.is_empty());

    write(root.path(), "hosts/server/Caddyfile", CADDY_UPDATED);
    let changed = plan_host(&repo, selector, &registry, &mut NoTargetQueries).unwrap();
    assert_ne!(changed.plan.id, safe.plan.id);
    let [write, notify] = &changed.plan.actions[..] else {
        panic!("expected write and reload")
    };
    assert!(matches!(&write.kind, ActionKind::WriteFile { path, .. } if path == CADDY_PATH));
    assert!(
        matches!(&notify.kind, ActionKind::NotifyService { unit, .. } if unit.name == "caddy.service")
    );
    assert_eq!(notify.depends_on, std::slice::from_ref(&write.id));
    let mut output = Vec::new();
    write_plan(&mut output, &changed.name, &changed.plan, true).unwrap();
    let json: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["plan_id"], changed.plan.id);
    assert_eq!(
        json["actions"],
        serde_json::to_value(&changed.plan.actions).unwrap()
    );
    let mut output = Vec::new();
    write_plan(&mut output, &changed.name, &changed.plan, false).unwrap();
    let text = String::from_utf8(output).unwrap();
    for part in [
        "更新文件",
        "重载服务",
        write.reason.as_str(),
        notify.reason.as_str(),
    ] {
        assert!(text.contains(part), "{text}");
    }
    memory.set_file(CADDY_PATH, digest(CADDY_UPDATED.as_bytes()));
    let converged = plan_host(&repo, selector, &registry, &mut NoTargetQueries).unwrap();
    assert!(converged.plan.actions.is_empty());
    assert_eq!(memory.executes.get(), 0);
}

#[test]
fn cli_fails_for_unknown_mistyped_and_conflicting_declarations_without_printing_a_plan() {
    let root = tempfile::tempdir().unwrap();
    caddy_repo(root.path());

    let run_failure = |host: &str, parts: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_dek"))
            .args(["plan", "-H", host, "--json", "--repo"])
            .arg(root.path())
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8(output.stderr).unwrap();
        for expected in parts {
            assert!(stderr.contains(expected), "{stderr}");
        }
    };
    run_failure("server", &["missing Package capability", "default.json"]);
    run_failure("server-remote", &["SSH", "lab-server"]);

    write(
        root.path(),
        "hosts/server/default.rb",
        "\nconfigure 'invalid.unknown', true\n",
    );
    run_failure("server-ruby", &["invalid.unknown", "default.rb:2"]);

    write_json(
        root.path(),
        "hosts/server/default.json",
        &[],
        json!([
            {"path":"services.caddy.enable","value":"yes"}
        ]),
    );
    run_failure(
        "server",
        &["services.caddy.enable", "default.json", "/options/0"],
    );

    write(
        root.path(),
        "profiles/other.rb",
        "\nservice :caddy, enable: true\n",
    );
    write_json(
        root.path(),
        "hosts/server/default.json",
        &["base", "other"],
        json!([]),
    );
    run_failure(
        "server",
        &[
            "services.caddy.enable",
            "profiles/base.json",
            "/options/0",
            "profiles/other.rb:2",
        ],
    );
}

#[test]
fn copied_cli_runs_embedded_ruby_outside_checkout_and_json_without_ruby() {
    let root = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let installed = root
        .path()
        .join(format!("dek{}", std::env::consts::EXE_SUFFIX));
    fs::copy(env!("CARGO_BIN_EXE_dek"), &installed).unwrap();
    let target = root.path().join("local.txt");
    let source = root.path().join("source.txt");
    fs::write(&target, "existing").unwrap();
    fs::write(&source, "planned").unwrap();
    write(
        root.path(),
        "dek.json",
        r#"{
      "schemaVersion":1,
      "hosts":{
        "server":{"entry":"hosts/server.rb","target":{"local":true}},
        "alias":{"entry":"hosts/server.rb","target":{"local":true}},
        "static":{"entry":"hosts/static.json","target":{"local":true}}
      },
      "modules":{"lab.file":{"schema":"modules/file/options.json","expand":"modules/file/expand.rb"}}
    }"#,
    );
    write(
        root.path(),
        "modules/file/options.json",
        r#"{"options":[
      {"path":"lab.file.enable","type":"boolean","default":false,"description":"Enable file module"},
      {"path":"lab.file.path","type":"string","description":"Target path"},
      {"path":"lab.file.content","type":"string","description":"Content"},
      {"path":"lab.file.architecture","type":"string","description":"Architecture"}
    ]}"#,
    );
    write(
        root.path(),
        "modules/file/expand.rb",
        "file_resource(option('lab.file.path'), content: option('lab.file.content'))\n",
    );
    write(
        root.path(),
        "hosts/server.rb",
        format!(
            "configure 'lab.file', enable: target.capability('file'), path: {}, content: target.read_text({}), architecture: target.fact('architecture')\n",
            json!(target.to_str().unwrap()),
            json!(source.to_str().unwrap())
        ),
    );
    write_json(root.path(), "hosts/static.json", &[], json!([]));

    let command = |host: &str, json: bool| {
        let mut cmd = Command::new(&installed);
        cmd.current_dir(cwd.path())
            .arg("--repo")
            .arg(root.path())
            .args(["plan", "-H", host])
            .env("XDG_CACHE_HOME", root.path().join("cache"));
        #[cfg(target_os = "linux")]
        cmd.arg("--state-dir").arg(root.path().join("target-state"));
        if json {
            cmd.arg("--json");
        }
        cmd
    };
    let result = command("server", true).output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let json: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(json["host"], "server");
    assert_eq!(json["plan_id"].as_str().unwrap().len(), 64);
    assert_eq!(
        json["actions"][0]["kind"]["writeFile"]["path"],
        target.to_str().unwrap()
    );
    assert_eq!(
        json["actions"][0]["kind"]["writeFile"]["content"]["digest"],
        digest(b"planned").0
    );
    assert_eq!(json["usedInputs"].as_array().unwrap().len(), 3);
    assert_eq!(fs::read(&target).unwrap(), b"existing");

    let text = command("server", false).output().unwrap();
    assert!(
        text.status.success(),
        "{}",
        String::from_utf8_lossy(&text.stderr)
    );
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(
        text.contains(json["plan_id"].as_str().unwrap()) && text.contains("更新文件"),
        "{text}"
    );

    let result = command("static", false).env("PATH", "").output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stdout).contains("0 项变更"));

    let help = Command::new(&installed)
        .current_dir(cwd.path())
        .args(["plan", "--help"])
        .output()
        .unwrap();
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("--json"));

    #[cfg(target_os = "linux")]
    {
        use dek_target::state::{PlanBinding, StateStore};

        let state_path = root.path().join("target-state");
        let store = StateStore::open(&state_path).unwrap();
        let identity = serde_json::from_value(json["target"].clone()).unwrap();
        let binding = PlanBinding {
            repository_id: json["repositoryId"].as_str().unwrap(),
            logical_host: "server",
            target: &identity,
        };
        let plan_id = json["plan_id"].as_str().unwrap();
        let old_content = digest(b"planned");
        let old_object = state_path.join("objects").join(&old_content.0[7..]);
        let old_record = state_path.join("plans").join(format!("{plan_id}.mpk"));

        let saved = store.lock().unwrap().load_plan(plan_id, binding).unwrap();
        assert_eq!(saved.plan.id, plan_id);
        assert_eq!(
            json["actions"],
            serde_json::to_value(&saved.plan.actions).unwrap()
        );
        assert_eq!(saved.contents[&old_content], b"planned");
        let repo = Repository::load(root.path()).unwrap();
        let registry =
            dek_cli::local_target::local_registry(&dek_cli::local_target::LocalFileBackend)
                .unwrap();
        let mut provider = dek_cli::local_target::LocalEvaluationTarget(&registry);
        let unpublished = plan_host(
            &repo,
            HostSelector::Named("alias"),
            &registry,
            &mut provider,
        )
        .unwrap();
        let wrong_bytes = BTreeMap::from([(old_content.clone(), b"corrupt".to_vec())]);
        let error = store
            .lock()
            .unwrap()
            .save_plan(
                "alias",
                &unpublished.plan,
                &unpublished.expanded.resources,
                &wrong_bytes,
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains(&old_content.0), "{error}");
        assert!(
            !state_path
                .join("plans")
                .join(format!("{}.mpk", unpublished.plan.id))
                .exists()
        );

        let alias = command("alias", true).output().unwrap();
        assert!(
            alias.status.success(),
            "{}",
            String::from_utf8_lossy(&alias.stderr)
        );
        let alias: Value = serde_json::from_slice(&alias.stdout).unwrap();
        let alias_id = alias["plan_id"].as_str().unwrap();
        assert_ne!(plan_id, alias_id);
        let alias = store
            .lock()
            .unwrap()
            .load_plan(
                alias_id,
                PlanBinding {
                    logical_host: "alias",
                    ..binding
                },
            )
            .unwrap();
        assert_eq!(alias.contents[&old_content], b"planned");
        assert_eq!(fs::read_dir(state_path.join("objects")).unwrap().count(), 1);

        fs::write(&source, b"new source").unwrap();
        fs::remove_dir_all(root.path().join("cache")).unwrap();
        fs::write(&target, b"new source").unwrap();
        let latest = command("server", true).output().unwrap();
        assert!(
            latest.status.success(),
            "{}",
            String::from_utf8_lossy(&latest.stderr)
        );
        let latest: Value = serde_json::from_slice(&latest.stdout).unwrap();
        let newer_id = latest["plan_id"].as_str().unwrap();
        assert_ne!(newer_id, plan_id);
        assert!(latest["actions"].as_array().unwrap().is_empty());
        let locked = store.lock().unwrap();
        let converged = locked.load_plan(newer_id, binding).unwrap();
        assert_eq!(converged.contents[&digest(b"new source")], b"new source");
        assert_eq!(
            locked.load_plan(plan_id, binding).unwrap().contents[&old_content],
            b"planned"
        );

        let bytes = fs::read(&old_record).unwrap();
        let mut wrong: Value = rmp_serde::from_slice(&bytes).unwrap();
        wrong["plan"]["actions"][0]["kind"] = json!({"unknownAction":{}});
        fs::write(&old_record, rmp_serde::to_vec_named(&wrong).unwrap()).unwrap();
        let error = locked.load_plan(plan_id, binding).unwrap_err().to_string();
        assert!(
            error.contains(plan_id) && error.contains("unknownAction"),
            "{error}"
        );
        let mut wrong: Value = rmp_serde::from_slice(&bytes).unwrap();
        wrong["formatVersion"] = json!(2);
        fs::write(&old_record, rmp_serde::to_vec_named(&wrong).unwrap()).unwrap();
        let error = locked.load_plan(plan_id, binding).unwrap_err().to_string();
        assert!(
            error.contains(plan_id) && error.contains("不支持的计划记录版本 2"),
            "{error}"
        );
        fs::write(&old_record, bytes).unwrap();
        fs::remove_file(&old_object).unwrap();
        let error = locked.load_plan(plan_id, binding).unwrap_err().to_string();
        assert!(error.contains(&old_content.0), "{error}");
        assert_eq!(fs::read(&target).unwrap(), b"new source");
    }
}
