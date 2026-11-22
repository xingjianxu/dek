#![cfg(target_os = "linux")]

use std::{
    cell::RefCell,
    env, fs,
    io::Write,
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
    ActionContext, ActionReadiness, Backend, BackendError, BackendRegistry, NotificationError,
    PlannedAction, PlannedNotification,
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
    fn remote_trace(&self, event: std::fmt::Arguments<'_>) -> Result<(), BackendError> {
        if self.mode == "remote-probe" {
            let mut log = fs::OpenOptions::new()
                .append(true)
                .create(true)
                .open(self.root.join("events"))
                .map_err(|error| BackendError(error.to_string()))?;
            writeln!(log, "{event}").map_err(|error| BackendError(error.to_string()))?;
        }
        Ok(())
    }

    fn package_present(&self, name: &str) -> bool {
        self.mode == "remote-probe" && self.root.join(format!("package-{name}")).is_file()
    }

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
        if self.mode.starts_with("notice-") || self.mode == "remote-probe" {
            capabilities.push(ResourceCapability {
                kind: ResourceKind::SystemdUnit,
                version: 1,
                attributes: ["reload".to_owned()].into(),
            });
        }
        if matches!(self.mode.as_str(), "package-probe" | "remote-probe") {
            capabilities.push(ResourceCapability {
                kind: ResourceKind::Package,
                version: 1,
                attributes: Default::default(),
            });
        }
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
        if matches!(self.mode.as_str(), "package-probe" | "remote-probe")
            && matches!(resource, DesiredResource::Package(_))
        {
            return Ok(());
        }
        LocalFileBackend.validate(resource)
    }

    fn observe(&self, resource: &DesiredResource) -> Result<Vec<Precondition>, BackendError> {
        if let DesiredResource::Package(package) = resource {
            return Ok(vec![Precondition::PackageVersion {
                package: package.package.clone(),
                expected: self
                    .package_present(&package.package.name)
                    .then(|| "1.0".into()),
            }]);
        }
        LocalFileBackend.observe(resource)
    }

    fn plan(
        &self,
        resource: &DesiredResource,
        observed: &[Precondition],
    ) -> Result<Vec<PlannedAction>, BackendError> {
        if let DesiredResource::Package(package) = resource {
            if self.package_present(&package.package.name) {
                return Ok(Vec::new());
            }
            return Ok(vec![PlannedAction {
                kind: ActionKind::InstallPackage {
                    package: package.package.clone(),
                    candidate_version: "1.0".into(),
                },
                reason: "probe package absent".into(),
                content_changed: false,
                preconditions: observed.to_vec(),
            }]);
        }
        LocalFileBackend.plan(resource, observed)
    }

    fn plan_notification(
        &self,
        notice: &dek_core::ServiceNotification,
        _: bool,
    ) -> Result<Option<PlannedNotification>, BackendError> {
        Ok(Some(PlannedNotification {
            reason: "probe service reload".into(),
            preconditions: vec![Precondition::UnitRunning {
                unit: notice.unit.clone(),
                expected: true,
            }],
        }))
    }

    fn validate_action(&self, action: &Action) -> Result<(), BackendError> {
        match &action.kind {
            ActionKind::WriteFile { .. }
            | ActionKind::InstallPackage { .. }
            | ActionKind::NotifyService { .. } => Ok(()),
            _ => Err(BackendError("probe can only write files".into())),
        }
    }

    fn inspect_action(&self, action: &Action) -> Result<ActionReadiness, BackendError> {
        if matches!(action.kind, ActionKind::NotifyService { .. }) {
            if self.mode == "notice-file-drift" {
                fs::write(self.root.join("first"), b"drift").unwrap();
            }
            return Ok(ActionReadiness::Ready);
        }
        if let ActionKind::InstallPackage { package, .. } = &action.kind {
            self.events
                .borrow_mut()
                .push(format!("observe {}", package.name));
            return Ok(if self.package_present(&package.name) {
                ActionReadiness::Satisfied
            } else {
                ActionReadiness::Ready
            });
        }
        let (path, content) = write_action(&action.kind);
        if self.mode == "interrupt-during-inspection" {
            std::process::exit(76);
        }
        if self.mode == "observe-first" && Path::new(path).ends_with("first") {
            self.events.borrow_mut().push(format!("observe {path}"));
        }
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

    fn requires_terminal(&self, action: &Action) -> bool {
        self.mode == "remote-probe"
            && matches!(
                &action.kind,
                ActionKind::InstallPackage { package, .. } if package.name == "terminal-probe"
            )
    }

    fn execute(&self, context: &ActionContext<'_>) -> Result<(), BackendError> {
        if let ActionKind::NotifyService { unit, .. } = &context.action.kind {
            self.events
                .borrow_mut()
                .push(format!("notify {}", unit.name));
            self.remote_trace(format_args!("notify {}", unit.name))?;
            if self.mode == "remote-probe" && self.root.join("fail-notice").exists() {
                fs::remove_file(self.root.join("fail-notice"))
                    .map_err(|error| BackendError(error.to_string()))?;
                return Err(BackendError("远端测试服务首次失败".into()));
            }
            if self.mode == "notice-loses-receipt" {
                return Err(BackendError("probe lost notification receipt".into()));
            }
            return Ok(());
        }
        if let ActionKind::InstallPackage { package, .. } = &context.action.kind
            && self.mode == "remote-probe"
        {
            if package.name == "terminal-probe" {
                let command = dek_target::terminal::TerminalCommand::new(
                    self.root.join("terminal-helper").into_os_string(),
                    std::iter::empty::<String>(),
                );
                context.run_interactive(&command)?;
            }
            self.remote_trace(format_args!("install {}", package.name))?;
            return fs::write(self.root.join(format!("package-{}", package.name)), "1.0")
                .map_err(|error| BackendError(error.to_string()));
        }
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
        self.remote_trace(format_args!("begin {path}"))?;
        if self.mode == "remote-probe" && path.contains("/slow-") {
            thread::sleep(Duration::from_millis(800));
        }
        if self.mode == "hold" {
            wait_for(&self.root.join("release"));
        }
        if self.mode == "fail-second" && Path::new(path).ends_with("second") {
            return Err(BackendError("probe second write failed".into()));
        }
        if self.mode == "notice-interrupt-before-write" {
            drop(_active);
            std::process::exit(75);
        }
        self.events.borrow_mut().push(format!("write {path}"));
        self.remote_trace(format_args!("write {path}"))?;
        fs::write(path, context.content(&content.digest)?)
            .map_err(|error| BackendError(error.to_string()))?;
        if self.mode == "interrupt-after-write" || self.root.join("interrupt").exists() {
            drop(_active);
            std::process::exit(75);
        }
        self.remote_trace(format_args!("end {path}"))?;
        Ok(())
    }

    fn execute_notification(&self, context: &ActionContext<'_>) -> Result<(), NotificationError> {
        self.execute(context).map_err(|error| {
            if self.mode == "remote-probe" {
                NotificationError::JobFailed(error)
            } else {
                NotificationError::Unverified(error)
            }
        })
    }

    fn verify(&self, action: &ActionKind) -> Result<bool, BackendError> {
        if let ActionKind::NotifyService { unit, .. } = action {
            self.events
                .borrow_mut()
                .push(format!("verify {}", unit.name));
            return Ok(true);
        }
        if let ActionKind::InstallPackage { package, .. } = action {
            return Ok(self.package_present(&package.name));
        }
        let (path, content) = write_action(action);
        self.events.borrow_mut().push(format!("verify {path}"));
        self.remote_trace(format_args!("verify {path}"))?;
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
        self.plan_mode("")
    }

    fn plan_mode(&self, mode: &str) -> String {
        let report = self.run(mode, &["plan", "--json"]);
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
fn serve_remote_probe() {
    let Some(root) = env::var_os("DEK_TEST_SSH_PROBE_ROOT") else {
        return;
    };
    let probe = Probe {
        root: root.into(),
        mode: "remote-probe".into(),
        events: RefCell::new(Vec::new()),
    };
    // The SSH wrapper directs the test harness's own stdout to stderr.
    let output = fs::OpenOptions::new()
        .write(true)
        .open("/proc/self/fd/3")
        .unwrap();
    command::run(["dek", "__target-stdio"], output, || Ok(probe.registry())).unwrap();
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
            dek_cli::applying::ApplySelection {
                plan: id,
                ..Default::default()
            },
            |_, _, _| Ok(LoadedPlan::Expired(dek_core::PlanMode::Normal)),
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

    let repo = fixture.repo().canonicalize().unwrap();
    let repository_id = CacheDigest::of_bytes(repo.as_os_str().as_encoded_bytes()).to_string();
    let registry = local_registry(&LocalFileBackend).unwrap();
    let store = StateStore::open(&fixture.state()).unwrap();
    let run = store
        .lock()
        .unwrap()
        .begin_run(
            &id,
            PlanBinding {
                repository_id: &repository_id,
                logical_host: "server",
                target: registry.target(),
            },
            "track-input-on-resume",
        )
        .unwrap();
    let resumed = fixture.run("", &["apply", "--run", &run.id]);
    assert!(stdout(&resumed).contains(replacement), "{resumed}");
    assert!(error(&resumed).contains(replacement), "{resumed}");
    assert!(events(&resumed).is_empty());
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

fn run_id(report: &Value) -> &str {
    stdout(report)
        .lines()
        .find_map(|line| line.strip_prefix("run_id "))
        .unwrap_or_else(|| panic!("missing run ID: {report}"))
}

fn status_for(fixture: &Fixture, run: Option<&str>) -> Value {
    let mut args = vec!["status", "--json"];
    if let Some(id) = run {
        args.extend_from_slice(&["--run", id]);
    }
    let report = fixture.run("", &args);
    assert!(report["error"].is_null(), "{report}");
    serde_json::from_str(stdout(&report)).unwrap()
}

#[test]
fn status_reports_partial_execution_and_plain_apply_resumes_only_the_failed_step() {
    let fixture = Fixture::new();
    let plan_id = fixture.plan();
    let failed = fixture.run("fail-second", &["apply", "--plan", &plan_id]);
    assert!(
        error(&failed).contains("probe second write failed"),
        "{failed}"
    );
    let id = run_id(&failed);
    let partial = status_for(&fixture, Some(id));
    assert_eq!(partial["run_id"], id);
    assert_eq!(partial["plan_id"], plan_id);
    assert_eq!(partial["actions"][0]["result"]["verified"]["changed"], true);
    assert!(partial["actions"][1]["result"]["failed"]["message"].is_string());

    fs::remove_dir_all(fixture.root.path().join("cache")).unwrap();
    fs::remove_file(fixture.repo().join("host.json")).unwrap();
    assert_eq!(status_for(&fixture, Some(id)), partial);
    let path = fixture.state().join("plans").join(format!("{plan_id}.mpk"));
    let mut record: Value = rmp_serde::from_slice(&fs::read(&path).unwrap()).unwrap();
    record["createdAt"] = json!(1);
    fs::write(path, rmp_serde::to_vec_named(&record).unwrap()).unwrap();

    let resumed = fixture.run("", &["apply"]);
    assert!(resumed["error"].is_null(), "{resumed}");
    assert_eq!(run_id(&resumed), id);
    assert!(stdout(&resumed).contains("恢复"), "{resumed}");
    assert_eq!(
        events(&resumed),
        [
            format!("write {}", fixture.target("second")),
            format!("verify {}", fixture.target("second")),
        ]
    );
    assert_eq!(fs::read(fixture.target("first")).unwrap(), b"A");
    let finished = status_for(&fixture, None);
    assert_eq!(finished["run_id"], id);
    assert_eq!(finished["finished"], true);
    assert_eq!(finished["actions"][1]["attempt"], 2);
}

#[test]
fn replay_survives_review_collection_and_status_distinguishes_previous_runs() {
    let fixture = Fixture::new();
    let plan = fixture.plan();
    let args = ["apply", "--plan", &plan, "--request-id", "job-1"];
    let first = fixture.run("", &args);
    assert!(first["error"].is_null(), "{first}");
    let original = status_for(&fixture, None);
    let first_id = run_id(&first);
    assert_eq!(original["run_id"], first_id);
    assert_eq!(original["request_id"], "job-1");
    fs::remove_file(fixture.state().join("plans").join(format!("{plan}.mpk"))).unwrap();

    let replay = fixture.run("", &args);
    assert!(replay["error"].is_null(), "{replay}");
    assert_eq!(run_id(&replay), first_id);
    assert!(events(&replay).is_empty());
    let explicit = fixture.run("", &["apply", "--run", first_id]);
    assert!(explicit["error"].is_null(), "{explicit}");
    assert_eq!(run_id(&explicit), first_id);
    assert!(events(&explicit).is_empty());

    let latest = fixture.run("", &["apply"]);
    assert!(latest["error"].is_null(), "{latest}");
    assert_ne!(run_id(&latest), first_id);
    assert_eq!(status_for(&fixture, None)["run_id"], run_id(&latest));
    assert_eq!(status_for(&fixture, Some(first_id)), original);
    let plan = status_for(&fixture, None)["plan_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let changed = fixture.run("", &["apply", "--plan", &plan, "--request-id", "job-1"]);
    assert!(error(&changed).contains(first_id), "{changed}");
    assert!(events(&changed).is_empty());

    for subcommand in ["status", "apply"] {
        let rejected = fixture.run("wrong-target", &[subcommand, "--run", first_id]);
        assert!(
            error(&rejected).contains("另一仓库或目标主机"),
            "{rejected}"
        );
        assert!(events(&rejected).is_empty());
    }
    let text = fixture.run("", &["status", "--run", first_id]);
    assert!(
        stdout(&text).contains("已核验") && stdout(&text).contains(&fixture.target("first")),
        "{text}"
    );
}

#[test]
fn real_interruption_reobserves_achieved_content_before_continuing() {
    let fixture = Fixture::new();
    let plan = fixture.plan();
    let args = ["apply", "--plan", &plan, "--request-id", "restart-probe"];
    let (child, _) = fixture.launch("interrupt-after-write", &args);
    let interrupted = child.wait_with_output().unwrap();
    assert_eq!(interrupted.status.code(), Some(75), "{interrupted:?}");
    let pending = status_for(&fixture, None);
    let id = pending["run_id"].as_str().unwrap();
    assert_eq!(pending["actions"][0]["result"], "unverified");
    assert_eq!(fs::read(fixture.target("first")).unwrap(), b"A");

    let recovered = fixture.run("observe-first", &args);
    assert!(recovered["error"].is_null(), "{recovered}");
    assert_eq!(run_id(&recovered), id);
    assert_eq!(
        events(&recovered),
        [
            format!("observe {}", fixture.target("first")),
            format!("write {}", fixture.target("second")),
            format!("verify {}", fixture.target("second")),
        ]
    );
    let finished = status_for(&fixture, Some(id));
    assert_eq!(finished["actions"][0]["attempt"], 1);
    assert_eq!(
        finished["actions"][0]["result"]["verified"]["changed"],
        false
    );
}

#[test]
fn an_uncertain_package_attempt_stays_unverified_and_is_not_dispatched() {
    let fixture = Fixture::new();
    fixture.write(
        "host.json",
        json!({
            "schemaVersion": 1,
            "imports": [],
            "options": [{"path": "environment.systemPackages", "value": ["recovery-probe"]}]
        })
        .to_string(),
    );
    let plan = fixture.plan_mode("package-probe");
    let repo = fixture.repo().canonicalize().unwrap();
    let repository_id = CacheDigest::of_bytes(repo.as_os_str().as_encoded_bytes()).to_string();
    let registry = local_registry(&LocalFileBackend).unwrap();
    let binding = PlanBinding {
        repository_id: &repository_id,
        logical_host: "server",
        target: registry.target(),
    };
    let store = StateStore::open(&fixture.state()).unwrap();
    let guard = store.lock().unwrap();
    let run = guard
        .begin_run(&plan, binding, "uncertain-package")
        .unwrap();
    assert!(matches!(
        run.actions[0].action.kind,
        ActionKind::InstallPackage { .. }
    ));
    guard
        .begin_action(&run.id, binding, &run.actions[0].action.id)
        .unwrap();
    drop(guard);

    let report = fixture.run("package-probe", &["apply", "--run", &run.id]);
    assert!(
        error(&report).contains("无法从当前状态确认包事务"),
        "{report}"
    );
    assert_eq!(events(&report), ["observe recovery-probe"]);
    let status = status_for(&fixture, Some(&run.id));
    assert_eq!(status["actions"][0]["result"], "unverified");
    assert_eq!(status["actions"][0]["attempt"], 1);
    assert!(
        status["actions"][0]["recoveryDiagnostic"]
            .as_str()
            .unwrap()
            .contains("包事务")
    );
    assert!(!Path::new(&fixture.target("first")).exists());
}

fn notice_fixture() -> Fixture {
    let fixture = Fixture::new();
    let path = fixture.target("first");
    fixture.write("expand.rb", format!(
        "file_resource {path:?}, source: 'source', notifications: [{{unit: {{scope: 'system', name: 'probe.service'}}, when: 'contentChanged', effect: 'reload'}}]\n"
    ));
    fixture
}

#[test]
fn later_plan_surfaces_the_original_queued_notice_and_recovery_completes_it() {
    let fixture = notice_fixture();
    let plan = fixture.plan_mode("notice-probe");
    let interrupt = fixture.root.path().join("interrupt");
    fs::write(&interrupt, b"exit before receipt").unwrap();
    let (child, _) = fixture.launch("notice-probe", &["apply", "--plan", &plan]);
    assert_eq!(child.wait_with_output().unwrap().status.code(), Some(75));
    fs::remove_file(interrupt).unwrap();

    let original = status_for(&fixture, None);
    let id = original["run_id"].as_str().unwrap();
    assert_eq!(
        original["pending_notifications"][0]["unit"]["name"],
        "probe.service"
    );
    let preview = fixture.run("notice-probe", &["plan", "--json"]);
    assert!(preview["error"].is_null(), "{preview}");
    let preview: Value = serde_json::from_str(stdout(&preview)).unwrap();
    assert_eq!(preview["actions"], json!([]));
    assert_eq!(preview["pending_notifications"][0]["runId"], id);
    assert_eq!(
        preview["pending_notifications"][0]["prerequisitesVerified"],
        false
    );

    let resumed = fixture.run("notice-probe", &["apply"]);
    assert!(resumed["error"].is_null(), "{resumed}");
    assert_eq!(run_id(&resumed), id);
    assert_eq!(
        events(&resumed),
        [
            format!("verify {}", fixture.target("first")),
            "notify probe.service".into(),
            "verify probe.service".into(),
        ]
    );
    assert_eq!(
        status_for(&fixture, Some(id))["pending_notifications"],
        json!([])
    );
}

#[test]
fn queued_notification_waits_for_the_reviewed_file_after_interruption_before_replacement() {
    let fixture = notice_fixture();
    fs::write(fixture.target("first"), b"old").unwrap();
    let plan = fixture.plan_mode("notice-probe");
    let (child, _) = fixture.launch("notice-interrupt-before-write", &["apply", "--plan", &plan]);
    assert_eq!(child.wait_with_output().unwrap().status.code(), Some(75));
    let before = status_for(&fixture, None);
    let id = before["run_id"].as_str().unwrap();
    assert_eq!(before["actions"][0]["result"], "unverified");
    assert_eq!(before["actions"][1]["attempt"], 0);
    assert_eq!(before["pending_notifications"].as_array().unwrap().len(), 1);
    assert_eq!(fs::read(fixture.target("first")).unwrap(), b"old");

    let resumed = fixture.run("notice-probe", &["apply"]);
    assert!(resumed["error"].is_null(), "{resumed}");
    assert_eq!(run_id(&resumed), id);
    assert_eq!(fs::read(fixture.target("first")).unwrap(), b"A");
    assert_eq!(
        events(&resumed).first().unwrap(),
        &format!("write {}", fixture.target("first"))
    );
    assert_eq!(
        events(&resumed)
            .iter()
            .filter(|event| event.starts_with("notify "))
            .count(),
        1
    );
    assert_eq!(
        status_for(&fixture, Some(id))["pending_notifications"],
        json!([])
    );
}

#[test]
fn notification_remains_pending_when_a_previously_verified_file_drifts() {
    let fixture = notice_fixture();
    let plan = fixture.plan_mode("notice-probe");
    let interrupted = fixture.run("notice-file-drift", &["apply", "--plan", &plan]);
    assert!(
        error(&interrupted).contains(&fixture.target("first")),
        "{interrupted}"
    );
    assert!(
        events(&interrupted)
            .iter()
            .all(|event| !event.starts_with("notify "))
    );
    let status = status_for(&fixture, None);
    assert_eq!(status["actions"][1]["attempt"], 0);
    assert_eq!(status["pending_notifications"].as_array().unwrap().len(), 1);
}

#[test]
fn a_lost_notification_receipt_stays_unverified_without_replay() {
    let fixture = notice_fixture();
    let plan = fixture.plan_mode("notice-probe");
    let attempted = fixture.run("notice-loses-receipt", &["apply", "--plan", &plan]);
    assert!(error(&attempted).contains("lost notification receipt"));
    let status = status_for(&fixture, None);
    assert_eq!(status["actions"][1]["result"], "unverified");

    let resumed = fixture.run("notice-probe", &["apply"]);
    assert!(error(&resumed).contains("无法从当前状态确认"));
    assert!(
        events(&resumed)
            .iter()
            .all(|event| !event.starts_with("notify "))
    );
    let status = status_for(&fixture, None);
    assert_eq!(status["actions"][1]["attempt"], 1);
    assert_eq!(status["pending_notifications"].as_array().unwrap().len(), 1);
}

#[test]
fn interruption_during_read_only_inspection_leaves_the_action_pending() {
    let fixture = Fixture::new();
    let plan = fixture.plan();
    let (child, _) = fixture.launch("interrupt-during-inspection", &["apply", "--plan", &plan]);
    assert_eq!(child.wait_with_output().unwrap().status.code(), Some(76));
    let pending = status_for(&fixture, None);
    assert_eq!(pending["actions"][0]["result"], "pending");
    assert_eq!(pending["actions"][0]["attempt"], 0);
    assert!(!Path::new(&fixture.target("first")).exists());

    let resumed = fixture.run("", &["apply"]);
    assert!(resumed["error"].is_null(), "{resumed}");
    assert_eq!(run_id(&resumed), pending["run_id"].as_str().unwrap());
    assert_eq!(events(&resumed).len(), 4);
    assert_eq!(status_for(&fixture, None)["actions"][0]["attempt"], 1);
}

#[test]
fn plan_accepts_explicit_aur_rebuild() {
    let output = Command::new(env!("CARGO_BIN_EXE_dek"))
        .args(["plan", "--upgrade", "--rebuild-aur", "--help"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("--rebuild-aur"));
}
