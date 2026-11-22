#![cfg(target_os = "linux")]

use std::{
    cell::RefCell,
    env, fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use dek_cli::{
    command,
    local_target::{LocalFileBackend, local_registry},
};
use dek_core::{
    Action, ActionKind, DesiredResource, Precondition, ResourceCapability, ResourceKind,
};
use dek_target::backend::{
    ActionContext, ActionReadiness, Backend, BackendError, BackendRegistry, PlannedAction,
};
use serde_json::{Value, json};
use tempfile::{NamedTempFile, TempDir};

use dek_controller::{
    cache::CacheDigest,
    repo::{HostSelector, Repository},
};
use dek_target::{
    executor::LoadedPlan,
    state::{ActionResult, PlanBinding, StateStore},
};

struct Probe {
    root: PathBuf,
    mode: String,
    events: RefCell<Vec<String>>,
}

impl Probe {
    fn registry(&self) -> BackendRegistry<'_> {
        // Reuse the production planner's target identity. Only execution is injected.
        let mut identity = local_registry(&LocalFileBackend).unwrap().target().clone();
        if self.mode == "wrong-target" {
            identity.id.push_str("-different");
        }
        let mut registry = BackendRegistry::new(identity, 1, |path| Ok(path.into()));
        registry.register(self).unwrap();
        registry
    }
}

impl Backend for Probe {
    fn capabilities(&self) -> Vec<ResourceCapability> {
        if self.mode == "without-file" {
            return Vec::new();
        }
        let mut capabilities = LocalFileBackend.capabilities();
        if self.mode == "extra-capability" {
            capabilities.push(ResourceCapability {
                kind: ResourceKind::Directory,
                version: 1,
                attributes: Default::default(),
            });
        }
        capabilities
    }

    fn validate(&self, resource: &DesiredResource) -> Result<(), BackendError> {
        LocalFileBackend.validate(resource)
    }

    fn observe(&self, resource: &DesiredResource) -> Result<Vec<Precondition>, BackendError> {
        LocalFileBackend.observe(resource)
    }

    fn plan(
        &self,
        resource: &DesiredResource,
        observed: &[Precondition],
    ) -> Result<Vec<PlannedAction>, BackendError> {
        LocalFileBackend.plan(resource, observed)
    }

    fn validate_action(&self, action: &Action) -> Result<(), BackendError> {
        match &action.kind {
            ActionKind::WriteFile { .. } => Ok(()),
            _ => Err(BackendError("probe can only write files".into())),
        }
    }

    fn inspect_action(&self, action: &Action) -> Result<ActionReadiness, BackendError> {
        let (path, content) = write_action(&action.kind);
        let resource = DesiredResource::File(dek_core::FileResource {
            path: path.into(),
            state: dek_core::FileState::Present(content.clone()),
        });
        let observed = self.observe(&resource)?;
        if self.plan(&resource, &observed)?.is_empty() {
            Ok(ActionReadiness::Satisfied)
        } else if observed == action.preconditions {
            Ok(ActionReadiness::Ready)
        } else {
            Ok(ActionReadiness::Conflict(format!(
                "probe file changed: {path}"
            )))
        }
    }

    fn execute(&self, context: &ActionContext<'_>) -> Result<(), BackendError> {
        let (path, content) = write_action(&context.action.kind);
        assert_eq!(Path::new(path).parent(), Some(self.root.as_path()));
        assert_eq!(self.inspect_action(context.action)?, ActionReadiness::Ready);
        let active = self.root.join("active");
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&active)
            .map_err(|error| BackendError(format!("overlapping execution: {error}")))?;
        let _active = Active(active);
        if self.mode == "hold" {
            wait_for(&self.root.join("release"));
        }
        self.events.borrow_mut().push(format!("write {path}"));
        fs::write(path, context.content(&content.digest)?)
            .map_err(|error| BackendError(error.to_string()))
    }

    fn verify(&self, action: &ActionKind) -> Result<bool, BackendError> {
        let (path, content) = write_action(action);
        self.events.borrow_mut().push(format!("verify {path}"));
        if self.mode == "fail-verify" {
            return Ok(false);
        }
        let resource = DesiredResource::File(dek_core::FileResource {
            path: path.into(),
            state: dek_core::FileState::Present(content.clone()),
        });
        Ok(self.plan(&resource, &self.observe(&resource)?)?.is_empty())
    }
}

fn write_action(action: &ActionKind) -> (&str, &dek_core::FileContent) {
    let ActionKind::WriteFile { path, content } = action else {
        panic!("unexpected action {action:?}");
    };
    (path, content)
}

struct Fixture {
    root: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let fixture = Self { root };
        fs::create_dir(fixture.repo()).unwrap();
        fixture.write(
            "dek.json",
            json!({
                "schemaVersion": 1,
                "hosts": {"server": {"entry": "host.json", "target": {"local": true}}},
                "modules": {"lab.file": {"schema": "options.json", "expand": "expand.rb"}}
            })
            .to_string(),
        );
        fixture.write(
            "host.json",
            r#"{"schemaVersion":1,"imports":[],"options":[]}"#,
        );
        fixture.write("options.json", r#"{"options":[{"path":"lab.file.enable","type":"boolean","default":true,"description":"file probe"}]}"#);
        fixture.write("source", "A");
        let first = fixture.target("first");
        let second = fixture.target("second");
        fixture.write("expand.rb", format!(
            "file_resource {second:?}, content: 'second', depends_on: [{{path: {first:?}}}]\nfile_resource {first:?}, source: 'source'\n",
        ));
        fixture
    }

    fn repo(&self) -> PathBuf {
        self.root.path().join("repo")
    }

    fn write(&self, path: &str, contents: impl AsRef<[u8]>) {
        fs::write(self.repo().join(path), contents).unwrap();
    }

    fn target(&self, name: &str) -> String {
        self.root.path().join(name).to_str().unwrap().into()
    }

    fn state(&self) -> PathBuf {
        self.root.path().join("state")
    }

    fn launch(&self, mode: &str, args: &[&str]) -> (Child, NamedTempFile) {
        let repo = self.repo();
        let state = self.state();
        let mut arguments = vec!["dek", "--repo", repo.to_str().unwrap(), "-H", "server"];
        arguments.extend_from_slice(args);
        arguments.extend_from_slice(&["--state-dir", state.to_str().unwrap()]);
        let report = NamedTempFile::new_in(self.root.path()).unwrap();
        let child = Command::new(env::current_exe().unwrap())
            .args(["--exact", "run_probe"])
            .env("DEK_TEST_APPLY_ROOT", self.root.path())
            .env(
                "DEK_TEST_APPLY_ARGS",
                serde_json::to_string(&arguments).unwrap(),
            )
            .env("DEK_TEST_APPLY_MODE", mode)
            .env("DEK_TEST_APPLY_REPORT", report.path())
            .env("XDG_CACHE_HOME", self.root.path().join("cache"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        (child, report)
    }

    fn run(&self, mode: &str, args: &[&str]) -> Value {
        result(self.launch(mode, args))
    }

    fn plan(&self) -> String {
        let report = self.run("", &["plan", "--json"]);
        assert!(report["error"].is_null(), "{report}");
        let plan: Value = serde_json::from_str(stdout(&report)).unwrap();
        plan["plan_id"].as_str().unwrap().into()
    }

    fn latest(&self) -> dek_target::state::RunRecord {
        let repo = self.repo().canonicalize().unwrap();
        let repository_id = CacheDigest::of_bytes(repo.as_os_str().as_encoded_bytes()).to_string();
        let registry = local_registry(&LocalFileBackend).unwrap();
        StateStore::open(&self.state())
            .unwrap()
            .lock()
            .unwrap()
            .latest_run(PlanBinding {
                repository_id: &repository_id,
                logical_host: "server",
                target: registry.target(),
            })
            .unwrap()
            .unwrap()
    }
}

struct Active(PathBuf);

impl Drop for Active {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn wait_for(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(path.exists(), "timed out waiting for {}", path.display());
}

fn result(invocation: (Child, NamedTempFile)) -> Value {
    let (child, report) = invocation;
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    serde_json::from_slice(&fs::read(report.path()).unwrap()).unwrap()
}

fn stdout(report: &Value) -> &str {
    report["output"].as_str().unwrap()
}

fn error(report: &Value) -> &str {
    report["error"]
        .as_str()
        .unwrap_or_else(|| panic!("expected error: {report}"))
}

fn events(report: &Value) -> Vec<&str> {
    report["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect()
}

#[test]
fn run_probe() {
    let Some(root) = env::var_os("DEK_TEST_APPLY_ROOT") else {
        return;
    };
    let arguments: Vec<String> =
        serde_json::from_str(&env::var("DEK_TEST_APPLY_ARGS").unwrap()).unwrap();
    let mode = env::var("DEK_TEST_APPLY_MODE").unwrap();
    let probe = Probe {
        root: root.into(),
        mode,
        events: RefCell::new(Vec::new()),
    };
    let mut output = Vec::new();
    let result = if probe.mode == "expired" {
        let repo = Repository::load(&probe.root.join("repo")).unwrap();
        let id = arguments
            .windows(2)
            .find(|pair| pair[0] == "--plan")
            .map(|pair| pair[1].as_str());
        dek_cli::applying::apply_local_with_loader(
            &mut output,
            &repo,
            HostSelector::Named("server"),
            &probe.registry(),
            &probe.root.join("state"),
            id,
            |_, _, _| Ok(LoadedPlan::Expired),
        )
    } else {
        if probe.mode == "contender" {
            fs::write(probe.root.join("contender"), b"ready").unwrap();
        }
        command::run(arguments, &mut output, || Ok(probe.registry()))
    };
    fs::write(
        env::var_os("DEK_TEST_APPLY_REPORT").unwrap(),
        json!({
            "output": String::from_utf8(output).unwrap(),
            "error": result.err().map(|error| error.to_string()),
            "events": probe.events.into_inner()
        })
        .to_string(),
    )
    .unwrap();
}

#[test]
fn cli_executes_frozen_content_in_dependency_order_and_repeats_without_writes() {
    let fixture = Fixture::new();
    let id = fixture.plan();
    fixture.write("source", "B");
    let report = fixture.run("", &["apply", "--plan", &id]);
    assert!(report["error"].is_null(), "{report}");
    let run = fixture.latest();
    assert!(
        stdout(&report).contains(&format!("run_id {}", run.id)),
        "{report}"
    );
    assert!(run.finished);
    assert_eq!(fs::read(fixture.target("first")).unwrap(), b"A");
    assert_eq!(fs::read(fixture.target("second")).unwrap(), b"second");
    assert_eq!(
        events(&report),
        [
            format!("write {}", fixture.target("first")),
            format!("verify {}", fixture.target("first")),
            format!("write {}", fixture.target("second")),
            format!("verify {}", fixture.target("second")),
        ]
    );

    fixture.write("source", "A");
    let report = fixture.run("", &["apply"]);
    assert!(report["error"].is_null(), "{report}");
    assert!(stdout(&report).contains("实际变更 0 项"), "{report}");
    assert!(events(&report).is_empty());
}

#[test]
fn cli_preserves_conflicts_and_reports_a_new_plan_when_scope_grows_or_expires() {
    let fixture = Fixture::new();
    fs::write(fixture.target("second"), b"second").unwrap();
    let id = fixture.plan();
    fs::write(fixture.target("first"), b"C").unwrap();

    let conflict = fixture.run("", &["apply", "--plan", &id]);
    assert!(
        error(&conflict).contains(&fixture.target("first")),
        "{conflict}"
    );
    assert_eq!(fs::read(fixture.target("first")).unwrap(), b"C");
    assert!(events(&conflict).is_empty());
    assert!(matches!(
        fixture.latest().actions[0].result,
        ActionResult::Failed { .. }
    ));

    fs::remove_file(fixture.target("first")).unwrap();
    fs::write(fixture.target("second"), b"drift").unwrap();
    for mode in ["", "expired"] {
        let report = fixture.run(mode, &["apply", "--plan", &id]);
        let replacement = stdout(&report)
            .lines()
            .find_map(|line| line.strip_prefix("计划 "))
            .unwrap_or_else(|| panic!("mode {mode}: {report}"));
        assert!(error(&report).contains(replacement), "{report}");
        assert_eq!(replacement.len(), 64);
        assert_ne!(replacement, id);
        assert!(!stdout(&report).contains("run_id"));
        assert!(!Path::new(&fixture.target("first")).exists());
        assert_eq!(fs::read(fixture.target("second")).unwrap(), b"drift");
        assert!(events(&report).is_empty());
    }
}

#[test]
fn execution_lease_serializes_two_processes_and_status_can_access_the_journal() {
    let fixture = Fixture::new();
    let id = fixture.plan();
    let first = fixture.launch("hold", &["apply", "--plan", &id]);
    wait_for(&fixture.root.path().join("active"));
    let lease = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(fixture.state().join("execution.lock"))
        .unwrap();
    assert!(
        lease.try_lock().is_err(),
        "first process did not hold the lease"
    );
    let pinned = fixture.latest();
    assert_eq!(pinned.plan_id, id);
    assert_eq!(pinned.actions[0].result, ActionResult::Unverified);

    let mut second = fixture.launch("contender", &["apply", "--plan", &id]);
    wait_for(&fixture.root.path().join("contender"));
    assert!(
        second.0.try_wait().unwrap().is_none(),
        "second process bypassed the lease"
    );
    fs::write(fixture.root.path().join("release"), b"ok").unwrap();
    let first = result(first);
    let second = result(second);
    assert!(
        first["error"].is_null() && second["error"].is_null(),
        "{first}\n{second}"
    );
    assert_eq!(events(&first).len(), 4);
    assert!(events(&second).is_empty());
    assert!(stdout(&second).contains("实际变更 0 项"));
    assert!(fixture.latest().finished);
}

#[test]
fn execution_rejects_wrong_binding_and_missing_capability_but_allows_unrelated_capability() {
    let fixture = Fixture::new();
    let id = fixture.plan();
    for (mode, cause) in [("wrong-target", "计划主机"), ("without-file", "File")] {
        let report = fixture.run(mode, &["apply", "--plan", &id]);
        assert!(error(&report).contains(cause), "{report}");
        assert!(!stdout(&report).contains("run_id"));
        assert!(events(&report).is_empty());
        assert!(!Path::new(&fixture.target("first")).exists());
    }
    let report = fixture.run("extra-capability", &["apply", "--plan", &id]);
    assert!(report["error"].is_null(), "{report}");
    assert_eq!(events(&report).len(), 4);
}

#[test]
fn verification_failure_records_the_attempt_and_stops_its_dependency() {
    let fixture = Fixture::new();
    let id = fixture.plan();
    let report = fixture.run("fail-verify", &["apply", "--plan", &id]);
    assert!(error(&report).contains("核验未通过"), "{report}");
    assert_eq!(events(&report).len(), 2);
    assert!(!Path::new(&fixture.target("second")).exists());
    let run = fixture.latest();
    assert!(!run.finished);
    assert!(matches!(run.actions[0].result, ActionResult::Failed { .. }));
    assert_eq!(run.actions[1].result, ActionResult::Pending);
}

#[test]
fn malformed_plan_and_missing_object_cannot_reach_execution() {
    for malformed in [false, true] {
        let fixture = Fixture::new();
        let id = fixture.plan();
        let expected = if malformed {
            let path = fixture.state().join("plans").join(format!("{id}.mpk"));
            let mut record: Value = rmp_serde::from_slice(&fs::read(&path).unwrap()).unwrap();
            record["plan"]["actions"][0]["kind"] = json!({"unexpectedAction": {}});
            fs::write(path, rmp_serde::to_vec_named(&record).unwrap()).unwrap();
            "unexpectedAction"
        } else {
            let digest = CacheDigest::of_bytes(b"A").to_string();
            fs::remove_file(fixture.state().join("objects").join(digest)).unwrap();
            "缺少内容对象"
        };
        let report = fixture.run("", &["apply", "--plan", &id]);
        assert!(error(&report).contains(expected), "{report}");
        assert!(error(&report).contains("dek plan"), "{report}");
        assert!(!stdout(&report).contains("run_id"));
        assert!(events(&report).is_empty());
        assert!(!Path::new(&fixture.target("first")).exists());
    }
}

#[test]
fn an_interrupted_attempt_is_observed_before_clearing_its_original_receipt() {
    use dek_target::{
        executor::{ExecutionOutcome, ExecutionRequest, Executor},
        state::ActionAttempt,
    };

    let fixture = Fixture::new();
    let plan_id = fixture.plan();
    let root = fixture.root.path();
    let probe = Probe {
        root: root.into(),
        mode: String::new(),
        events: RefCell::new(Vec::new()),
    };
    let registry = probe.registry();
    let store = StateStore::open(&fixture.state()).unwrap();
    let repo = fixture.repo().canonicalize().unwrap();
    let repository_id = CacheDigest::of_bytes(repo.as_os_str().as_encoded_bytes()).to_string();
    let binding = PlanBinding {
        logical_host: "server",
        repository_id: &repository_id,
        target: registry.target(),
    };
    let request_id = "interrupted-attempt";
    let run = store
        .lock()
        .unwrap()
        .begin_run(&plan_id, binding, request_id)
        .unwrap();
    let first = &run.actions[0].action.id;
    let attempt = store
        .lock()
        .unwrap()
        .begin_action(&run.id, binding, first)
        .unwrap();
    assert_eq!(attempt, ActionAttempt::Started(1));

    let executor = Executor::new(&store, &registry);
    let apply = || {
        executor.apply(
            ExecutionRequest {
                binding,
                plan_id: &plan_id,
                request_id,
            },
            |_| unreachable!("probe plan has no target inputs"),
            |id| {
                assert_eq!(id, run.id);
                Ok(())
            },
        )
    };
    let error = apply().unwrap_err().to_string();
    assert!(error.contains("待核验"), "{error}");
    assert!(probe.events.borrow().is_empty());
    assert_eq!(fixture.latest().actions[0].result, ActionResult::Unverified);

    fs::write(fixture.target("first"), b"A").unwrap();
    let ExecutionOutcome::Finished(recovered) = apply().unwrap() else {
        panic!("expected verified run on the same approval");
    };
    assert_eq!(recovered.id, run.id);
    assert_eq!(recovered.actions[0].attempt, 1);
    assert_eq!(
        recovered.actions[0].result,
        ActionResult::Verified { changed: false }
    );
    assert_eq!(
        *probe.events.borrow(),
        [
            format!("write {}", fixture.target("second")),
            format!("verify {}", fixture.target("second")),
        ]
    );
}

#[test]
fn a_changed_tracked_target_input_requires_review_of_a_fresh_plan() {
    let fixture = Fixture::new();
    let source = fixture.repo().join("dek.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(source).unwrap()).unwrap();
    manifest["hosts"]["server"]["entry"] = json!("host.rb");
    fixture.write("dek.json", manifest.to_string());
    let input = fixture.target("tracked");
    fixture.write(
        "host.rb",
        format!("configure 'lab.file.enable', target.read_text({input:?}) == 'yes'\n"),
    );
    fs::write(&input, b"yes").unwrap();
    let id = fixture.plan();
    fs::write(&input, b"no").unwrap();

    let report = fixture.run("", &["apply", "--plan", &id]);
    let out = stdout(&report);
    assert!(out.contains(&input) && out.contains("0 项变更"), "{report}");
    assert!(!out.contains(&id) && !out.contains("run_id"), "{report}");
    let replacement = out
        .lines()
        .find_map(|line| line.strip_prefix("计划 "))
        .unwrap();
    assert!(error(&report).contains(replacement), "{report}");
    assert!(events(&report).is_empty());
    assert!(!Path::new(&fixture.target("first")).exists());
}

#[test]
fn expired_approval_returns_the_newly_issued_id_even_when_desired_state_is_identical() {
    let fixture = Fixture::new();
    let expired_id = fixture.plan();
    let path = fixture
        .state()
        .join("plans")
        .join(format!("{expired_id}.mpk"));
    let mut record: Value = rmp_serde::from_slice(&fs::read(&path).unwrap()).unwrap();
    record["createdAt"] = json!(1);
    fs::write(path, rmp_serde::to_vec_named(&record).unwrap()).unwrap();

    let report = fixture.run("", &["apply", "--plan", &expired_id]);
    let out = stdout(&report);
    assert!(out.contains("已审阅计划过期"), "{report}");
    let fresh_id = out
        .lines()
        .find_map(|line| line.strip_prefix("计划 "))
        .unwrap();
    assert_ne!(fresh_id, expired_id);
    assert!(
        fixture
            .state()
            .join("plans")
            .join(format!("{fresh_id}.mpk"))
            .exists()
    );
    assert!(events(&report).is_empty());

    let report = fixture.run("", &["apply", "--plan", fresh_id]);
    assert!(report["error"].is_null(), "{report}");
    assert_eq!(events(&report).len(), 4);
}
