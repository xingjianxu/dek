#![cfg(target_os = "linux")]

use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::Path,
};

use dek_core::{
    ContentDigest, DesiredResource, FileAttributes, FileContent, FileResource, FileState, Resource,
    ResourceSet, TargetIdentity,
};
use dek_linux::filesystem::{FileBackend, FileValidationGroup, ValidationInput};
use dek_target::{
    backend::{BackendError, BackendRegistry},
    executor::{ExecutionOutcome, ExecutionRequest, Executor},
    planner::plan_for_host,
    state::{PlanBinding, StateStore},
};
use sha2::{Digest, Sha256};

#[test]
fn validates_the_complete_file_group_before_replacing_any_destination() {
    let fixture = tempfile::tempdir().unwrap();
    let managed = fixture.path().join("managed");
    fs::create_dir(&managed).unwrap();
    let paths = [managed.join("a.conf"), managed.join("b.conf")];
    fs::write(&paths[0], b"valid: old A").unwrap();
    fs::write(&paths[1], b"valid: old B").unwrap();
    let group = FileValidationGroup::new(
        paths.iter().map(|path| path.to_str().unwrap().to_owned()),
        valid_group,
    )
    .unwrap();
    let files = FileBackend::with_validation(vec![group]);
    let identity = TargetIdentity {
        id: "file-fixture".into(),
        os: "linux".into(),
        distribution: None,
        architecture: std::env::consts::ARCH.into(),
    };
    let mut registry = BackendRegistry::new(identity.clone(), 1, |path| Ok(path.into()));
    registry.register(&files).unwrap();
    let store = StateStore::open(&fixture.path().join("state")).unwrap();
    let binding = PlanBinding {
        repository_id: "workspace",
        logical_host: "file-test",
        target: &identity,
    };

    let apply = |content: [&'static [u8]; 2], request_id| {
        let (resources, contents) = group_fixture(&paths, content);
        let draft = plan_for_host(
            &registry,
            binding.repository_id,
            binding.logical_host,
            &resources,
            &[],
        )
        .unwrap();
        let plan = store
            .lock()
            .unwrap()
            .save_plan(binding.logical_host, &draft, &resources, &contents)
            .unwrap();
        Executor::new(&store, &registry).apply(
            ExecutionRequest {
                binding,
                plan_id: &plan.id,
                request_id,
            },
            |_| panic!("fixture uses no external inputs"),
            |_| Ok(()),
        )
    };

    let failure = apply([b"valid: new A", b"invalid B"], "failed").unwrap_err();
    assert!(failure.to_string().contains("b.conf"), "{failure}");
    assert_eq!(fs::read(&paths[0]).unwrap(), b"valid: old A");
    assert_eq!(fs::read(&paths[1]).unwrap(), b"valid: old B");
    assert_eq!(fs::read_dir(&managed).unwrap().count(), 2);

    assert!(matches!(
        apply([b"valid: new A", b"valid: new B"], "fixed").unwrap(),
        ExecutionOutcome::Finished(run) if run.finished
    ));
    assert_eq!(fs::read(&paths[0]).unwrap(), b"valid: new A");
    assert_eq!(fs::read(&paths[1]).unwrap(), b"valid: new B");
    assert_eq!(fs::read_dir(&managed).unwrap().count(), 2);

    // A converged group member still contributes the exact reviewed bytes.
    assert!(matches!(
        apply([b"valid: final A", b"valid: new B"], "extend").unwrap(),
        ExecutionOutcome::Finished(run) if run.finished
    ));
    assert_eq!(fs::read(&paths[0]).unwrap(), b"valid: final A");
    assert_eq!(fs::read(&paths[1]).unwrap(), b"valid: new B");
    assert_eq!(fs::read_dir(&managed).unwrap().count(), 2);
}

fn group_fixture(
    paths: &[impl AsRef<Path>],
    content: impl IntoIterator<Item = &'static [u8]>,
) -> (ResourceSet, BTreeMap<ContentDigest, Vec<u8>>) {
    let mut resources = ResourceSet::default();
    let mut contents = BTreeMap::new();
    for (path, bytes) in paths.iter().zip(content) {
        let digest = ContentDigest(format!("sha256:{:x}", Sha256::digest(bytes)));
        contents.insert(digest.clone(), bytes.to_vec());
        resources.resources.push(Resource {
            desired: DesiredResource::File(FileResource {
                path: path.as_ref().to_str().unwrap().into(),
                state: FileState::Present(FileContent {
                    digest,
                    attributes: FileAttributes::default(),
                }),
            }),
            origins: Vec::new(),
            depends_on: Vec::new(),
            notifications: Vec::new(),
        });
    }
    (resources, contents)
}

fn valid_group(inputs: &[ValidationInput]) -> Result<(), BackendError> {
    assert_eq!(inputs.len(), 2);
    for input in inputs {
        let mut file = &input.file;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert!(file.write_all(b"unexpected change").is_err());
        if !bytes.starts_with(b"valid: ") {
            return Err(BackendError(format!("invalid input {:?}", input.path)));
        }
    }
    Ok(())
}

#[test]
fn a_converged_file_that_changes_after_replanning_cannot_bypass_group_validation() {
    use std::cell::Cell;

    use dek_core::{Action, ActionKind, Precondition, ResourceCapability};
    use dek_target::backend::{ActionContext, ActionReadiness, Backend, PlannedAction};

    struct ChangingTarget {
        files: FileBackend,
        change_after_observation: Cell<bool>,
    }

    impl Backend for ChangingTarget {
        fn capabilities(&self) -> Vec<ResourceCapability> {
            self.files.capabilities()
        }

        fn validate(&self, resource: &DesiredResource) -> Result<(), BackendError> {
            self.files.validate(resource)
        }

        fn observe(&self, resource: &DesiredResource) -> Result<Vec<Precondition>, BackendError> {
            let observation = self.files.observe(resource)?;
            if self.change_after_observation.replace(false)
                && let DesiredResource::File(file) = resource
            {
                fs::write(&file.path, b"reviewed precondition").unwrap();
            }
            Ok(observation)
        }

        fn plan(
            &self,
            resource: &DesiredResource,
            observed: &[Precondition],
        ) -> Result<Vec<PlannedAction>, BackendError> {
            self.files.plan(resource, observed)
        }

        fn validate_action(&self, action: &Action) -> Result<(), BackendError> {
            self.files.validate_action(action)
        }

        fn inspect_action(&self, action: &Action) -> Result<ActionReadiness, BackendError> {
            self.files.inspect_action(action)
        }

        fn execute(&self, _context: &ActionContext<'_>) -> Result<(), BackendError> {
            panic!("the newly needed write was not validated")
        }

        fn verify(&self, action: &ActionKind) -> Result<bool, BackendError> {
            self.files.verify(action)
        }
    }

    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join("managed.conf");
    fs::write(&path, b"reviewed precondition").unwrap();
    let (resources, contents) = group_fixture(&[&path], [b"reviewed result".as_slice()]);
    let target = ChangingTarget {
        files: FileBackend::default(),
        change_after_observation: Cell::new(false),
    };
    let identity = TargetIdentity {
        id: "changing-fixture".into(),
        os: "linux".into(),
        distribution: None,
        architecture: std::env::consts::ARCH.into(),
    };
    let mut registry = BackendRegistry::new(identity.clone(), 1, |path| Ok(path.into()));
    registry.register(&target).unwrap();
    let state = StateStore::open(&fixture.path().join("state")).unwrap();
    let binding = PlanBinding {
        repository_id: "workspace",
        logical_host: "file-test",
        target: &identity,
    };
    let plan = plan_for_host(
        &registry,
        binding.repository_id,
        binding.logical_host,
        &resources,
        &[],
    )
    .unwrap();
    let plan = state
        .lock()
        .unwrap()
        .save_plan(binding.logical_host, &plan, &resources, &contents)
        .unwrap();
    fs::write(&path, b"reviewed result").unwrap();
    target.change_after_observation.set(true);

    let error = Executor::new(&state, &registry)
        .apply(
            ExecutionRequest {
                binding,
                plan_id: &plan.id,
                request_id: "changed-between-observations",
            },
            |_| panic!("fixture uses no external inputs"),
            |_| Ok(()),
        )
        .unwrap_err();
    assert!(error.to_string().contains("范围复核后"), "{error}");
    assert_eq!(fs::read(path).unwrap(), b"reviewed precondition");
}
