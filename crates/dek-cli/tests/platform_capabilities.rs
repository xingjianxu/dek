#![cfg(target_os = "linux")]

use std::{cell::RefCell, collections::BTreeMap, error::Error, fs, path::PathBuf};

use dek_cli::command;
use dek_core::{
    Action, ActionKind, ContentDigest, DesiredResource, FileContent, FileObservation, FileState,
    Precondition, ResourceCapability, ResourceKind, TargetIdentity,
};
use dek_linux::filesystem::DirectoryBackend;
use dek_target::backend::{
    ActionContext, ActionReadiness, Backend, BackendError, BackendRegistry, PlannedAction,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/platform-capabilities"
);
const RAW_PATH: &str = r"C:\Apps\demo.conf";
const SOURCE: &[u8] = include_bytes!("fixtures/platform-capabilities/source/demo.conf");

struct MemoryFiles {
    version: u32,
    mode: bool,
    files: RefCell<BTreeMap<String, FileContent>>,
}

impl MemoryFiles {
    fn new(version: u32, mode: bool) -> Self {
        Self {
            version,
            mode,
            files: RefCell::default(),
        }
    }

    fn registry(&self, directory: bool) -> BackendRegistry<'_> {
        let mut registry = BackendRegistry::new(
            TargetIdentity {
                id: "memory-files".into(),
                os: "windows".into(),
                distribution: None,
                architecture: "x86_64".into(),
            },
            1,
            path_key,
        );
        registry.register(self).unwrap();
        if directory {
            registry.register(&DirectoryBackend).unwrap();
        }
        registry
    }

    fn file(&self, raw: &str) -> Result<FileObservation, BackendError> {
        let key = path_key(raw)?;
        Ok(self
            .files
            .borrow()
            .get(&key)
            .cloned()
            .map_or(FileObservation::Missing, FileObservation::Present))
    }
}

// For the fixture's absolute drive paths, either slash and any ASCII case identify the same file.
fn path_key(raw: &str) -> Result<String, BackendError> {
    let path = raw.replace('\\', "/").to_ascii_lowercase();
    if path.starts_with("c:/") {
        Ok(path)
    } else {
        Err(BackendError(format!(
            "expected an absolute C: path: {raw:?}"
        )))
    }
}

impl Backend for MemoryFiles {
    fn capabilities(&self) -> Vec<ResourceCapability> {
        vec![ResourceCapability {
            kind: ResourceKind::File,
            version: self.version,
            attributes: self.mode.then_some("mode".to_owned()).into_iter().collect(),
        }]
    }

    fn validate(&self, resource: &DesiredResource) -> Result<(), BackendError> {
        let DesiredResource::File(file) = resource else {
            panic!("unexpected resource: {resource:?}");
        };
        path_key(&file.path)?;
        Ok(())
    }

    fn observe(&self, resource: &DesiredResource) -> Result<Vec<Precondition>, BackendError> {
        let DesiredResource::File(file) = resource else {
            panic!("unexpected resource: {resource:?}");
        };
        Ok(vec![Precondition::File {
            path: file.path.clone(),
            expected: self.file(&file.path)?,
        }])
    }

    fn plan(
        &self,
        resource: &DesiredResource,
        observed: &[Precondition],
    ) -> Result<Vec<PlannedAction>, BackendError> {
        let DesiredResource::File(file) = resource else {
            panic!("unexpected resource: {resource:?}");
        };
        let FileState::Present(content) = &file.state else {
            panic!("fixture only declares present files");
        };
        if self.file(&file.path)? == FileObservation::Present(content.clone()) {
            return Ok(Vec::new());
        }
        Ok(vec![PlannedAction {
            kind: ActionKind::WriteFile {
                path: file.path.clone(),
                content: content.clone(),
            },
            reason: "memory file differs".into(),
            preconditions: observed.to_vec(),
            content_changed: true,
        }])
    }

    fn validate_action(&self, action: &Action) -> Result<(), BackendError> {
        let (path, _) = write_file(&action.kind);
        path_key(path)?;
        Ok(())
    }

    fn inspect_action(&self, action: &Action) -> Result<ActionReadiness, BackendError> {
        let (path, _) = write_file(&action.kind);
        match self.file(path)? {
            FileObservation::Missing => Ok(ActionReadiness::Ready),
            _ if self.verify(&action.kind)? => Ok(ActionReadiness::Satisfied),
            _ => Ok(ActionReadiness::Conflict(path.into())),
        }
    }

    fn execute(&self, context: &ActionContext<'_>) -> Result<(), BackendError> {
        let (path, content) = write_file(&context.action.kind);
        assert_eq!(context.content(&content.digest)?, SOURCE);
        self.files
            .borrow_mut()
            .insert(path_key(path)?, content.clone());
        Ok(())
    }

    fn verify(&self, action: &ActionKind) -> Result<bool, BackendError> {
        let (path, content) = write_file(action);
        Ok(self.file(path)? == FileObservation::Present(content.clone()))
    }
}

fn write_file(kind: &ActionKind) -> (&str, &FileContent) {
    let ActionKind::WriteFile { path, content } = kind else {
        panic!("unexpected action {kind:?}");
    };
    (path, content)
}

struct Fixture(tempfile::TempDir);

impl Fixture {
    fn new() -> Self {
        let fixture = Self(tempfile::tempdir().unwrap());
        let root = fixture.0.path();
        fs::create_dir(root.join("source")).unwrap();
        for entry in fs::read_dir(FIXTURE).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_file() {
                fs::copy(entry.path(), root.join(entry.file_name())).unwrap();
            }
        }
        fs::write(root.join("source/demo.conf"), SOURCE).unwrap();
        fixture
    }

    fn run(
        &self,
        backend: &MemoryFiles,
        directory: bool,
        host: &str,
        args: &[&str],
        out: &mut Vec<u8>,
    ) -> Result<(), Box<dyn Error>> {
        let root = self.0.path().to_str().unwrap();
        let state = self.state();
        command::run(
            ["dek", "--repo", root, "-H", host]
                .into_iter()
                .chain(args.iter().copied())
                .chain(["--state-dir", state.to_str().unwrap()]),
            out,
            || Ok(backend.registry(directory)),
        )
    }

    fn state(&self) -> PathBuf {
        self.0.path().join("target")
    }
}

#[test]
fn shared_profile_plans_a_file_with_its_raw_target_path_and_controller_source() {
    let fixture = Fixture::new();
    let backend = MemoryFiles::new(1, true);
    let mut out = Vec::new();
    fixture
        .run(&backend, false, "memory", &["plan", "--json"], &mut out)
        .unwrap();
    let plan: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(plan["target"]["id"], "memory-files");
    assert_eq!(plan["target"]["os"], "windows");
    assert_eq!(plan["actions"].as_array().unwrap().len(), 1);
    let file = &plan["actions"][0]["kind"]["writeFile"];
    assert_eq!(file["path"], RAW_PATH);
    let digest = ContentDigest(format!("sha256:{:x}", Sha256::digest(SOURCE)));
    assert_eq!(file["content"]["digest"], digest.0);
    assert_eq!(plan["usedInputs"][0]["id"], "systemdUnit.content");
    assert_eq!(backend.file(RAW_PATH).unwrap(), FileObservation::Missing);
}

#[test]
fn mixed_declarations_fail_before_publishing_any_plan() {
    for (host, mode, parts) in [
        (
            "forced-timer",
            true,
            [
                "missing SystemdUnit capability",
                "dek-platform-review.timer",
                "timer.rb:1",
            ],
        ),
        (
            "memory",
            false,
            ["missing File capability attribute", "mode", "expand.rb:1"],
        ),
        (
            "alias",
            true,
            ["target resource alias", "c:/apps/demo.conf", "expand.rb:3"],
        ),
    ] {
        let fixture = Fixture::new();
        let backend = MemoryFiles::new(1, mode);
        let mut out = Vec::new();
        let error = fixture
            .run(&backend, false, host, &["plan", "--json"], &mut out)
            .unwrap_err()
            .to_string();
        assert!(error.contains("target memory-files"), "{error}");
        for part in parts {
            assert!(error.contains(part), "{error}");
        }
        assert!(out.is_empty());
        assert!(!fixture.state().exists());
        assert!(backend.files.borrow().is_empty());
    }
}

#[test]
fn execution_rechecks_the_reviewed_file_capability_before_any_action() {
    let fixture = Fixture::new();
    let original = MemoryFiles::new(1, true);
    let mut out = Vec::new();
    fixture
        .run(&original, false, "memory", &["plan", "--json"], &mut out)
        .unwrap();
    let plan: Value = serde_json::from_slice(&out).unwrap();
    let id = plan["plan_id"].as_str().unwrap();
    assert_eq!(
        plan["usedCapabilities"][0]["attributes"],
        serde_json::json!(["mode"])
    );
    let args = ["apply", "--plan", id];

    for (backend, reason) in [
        (MemoryFiles::new(1, false), "mode"),
        (MemoryFiles::new(2, true), "version"),
    ] {
        out.clear();
        let error = fixture
            .run(&backend, false, "memory", &args, &mut out)
            .unwrap_err()
            .to_string();
        assert!(error.contains("incompatible File capability"), "{error}");
        assert!(error.contains(reason), "{error}");
        assert!(out.is_empty(), "incompatible plan started a run");
        assert!(backend.files.borrow().is_empty());
        fixture
            .run(&backend, false, "memory", &["status", "--json"], &mut out)
            .unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&out).unwrap(), Value::Null);
    }

    out.clear();
    fixture
        .run(&original, true, "memory", &args, &mut out)
        .unwrap();
    assert!(String::from_utf8(out).unwrap().contains("run_id"));
    let file = original.file("c:/apps/DEMO.conf").unwrap();
    assert_eq!(file, original.file(RAW_PATH).unwrap());
    let FileObservation::Present(file) = file else {
        panic!("file was not written");
    };
    assert_eq!(file.attributes.mode.as_deref(), Some("0640"));
}
