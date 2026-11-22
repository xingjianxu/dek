#![cfg(target_os = "linux")]

use std::{collections::BTreeMap, env, fs, os::unix::fs::symlink, path::Path, process::Command};

use dek_core::{
    ContentDigest, DesiredResource, FileAttributes, FileContent, FileResource, FileState, Resource,
    ResourceSet, TargetIdentity,
};
use dek_target::{
    backend::BackendRegistry,
    planner::plan_for_host,
    state::{PlanBinding, StateStore},
};
use sha2::{Digest, Sha256};

fn wanted(bytes: &[u8]) -> (ContentDigest, ResourceSet) {
    let digest = ContentDigest(format!("sha256:{:x}", Sha256::digest(bytes)));
    let resources = ResourceSet {
        resources: vec![Resource {
            desired: DesiredResource::File(FileResource {
                path: "/src/same".into(),
                state: FileState::Present(FileContent {
                    digest: digest.clone(),
                    attributes: FileAttributes::default(),
                }),
            }),
            origins: Vec::new(),
            depends_on: Vec::new(),
            notifications: Vec::new(),
        }],
    };
    (digest, resources)
}

fn target() -> TargetIdentity {
    TargetIdentity {
        id: "machine".into(),
        os: "linux".into(),
        distribution: Some("arch".into()),
        architecture: "x86_64".into(),
    }
}

fn binding<'a>(host: &'a str, target: &'a TargetIdentity) -> PlanBinding<'a> {
    PlanBinding {
        repository_id: "repo",
        logical_host: host,
        target,
    }
}

#[test]
fn validates_content_bytes_and_refuses_paths_or_symlinks() {
    let storage = tempfile::tempdir().unwrap();
    let root = storage.path().join("state");
    let store = StateStore::open(&root).unwrap();
    let state = store.lock().unwrap();
    let (digest, _) = wanted(b"reviewed");
    assert!(!state.has_object(&digest).unwrap());
    assert!(state.put_object(&digest, b"wrong").is_err());
    state.put_object(&digest, b"reviewed").unwrap();
    state.put_object(&digest, b"reviewed").unwrap();
    assert_eq!(state.read_object(&digest).unwrap(), b"reviewed");
    assert!(
        state
            .has_object(&ContentDigest("sha256:../state.lock".into()))
            .is_err()
    );
    assert_eq!(fs::read_dir(root.join("objects")).unwrap().count(), 1);
    let file = root.join("objects").join(&digest.0[7..]);
    fs::remove_file(&file).unwrap();
    symlink("/etc/passwd", &file).unwrap();
    assert!(state.read_object(&digest).is_err());
    let alias = storage.path().join("alias");
    symlink(&root, &alias).unwrap();
    assert!(StateStore::open(&alias).is_err());
}

#[test]
fn binds_identical_plan_to_each_logical_host() {
    let storage = tempfile::tempdir().unwrap();
    let store = StateStore::open(&storage.path().join("state")).unwrap();
    let state = store.lock().unwrap();
    let target = target();
    let registry = BackendRegistry::new(target.clone(), 1, |path| Ok(path.into()));
    let resources = ResourceSet::default();
    let first = plan_for_host(&registry, "repo", "first", &resources, &[]).unwrap();
    let second = plan_for_host(&registry, "repo", "second", &resources, &[]).unwrap();
    assert_ne!(first.id, second.id);
    state
        .save_plan("first", &first, &resources, &BTreeMap::new())
        .unwrap();
    let loaded = state
        .load_plan(&first.id, binding("first", &target))
        .unwrap();
    assert_eq!(loaded.plan, first);
    assert_eq!(loaded.resources, resources);
    assert!(
        state
            .load_plan(&first.id, binding("second", &target))
            .unwrap_err()
            .to_string()
            .contains("second")
    );
    let mut wrong = target.clone();
    wrong.id = "another-machine".into();
    assert!(
        state
            .load_plan(&first.id, binding("first", &wrong))
            .is_err()
    );
    assert!(
        state
            .load_plan(
                &first.id,
                PlanBinding {
                    repository_id: "different-repo",
                    ..binding("first", &target)
                }
            )
            .is_err()
    );
}

#[test]
fn restrictive_umask_keeps_new_records_and_objects_readable() {
    const CHILD_ROOT: &str = "DEK_TEST_RESTRICTIVE_STATE_ROOT";
    if let Some(root) = env::var_os(CHILD_ROOT) {
        let store = StateStore::open(Path::new(&root)).unwrap();
        let state = store.lock().unwrap();
        let target = target();
        let registry = BackendRegistry::new(target.clone(), 1, |path| Ok(path.into()));
        let resources = ResourceSet::default();
        let plan = plan_for_host(&registry, "repo", "host", &resources, &[]).unwrap();
        let (digest, _) = wanted(b"reviewed");

        state
            .save_plan("host", &plan, &resources, &BTreeMap::new())
            .unwrap();
        state.put_object(&digest, b"reviewed").unwrap();
        assert_eq!(
            state
                .load_plan(&plan.id, binding("host", &target))
                .unwrap()
                .plan,
            plan
        );
        assert_eq!(state.read_object(&digest).unwrap(), b"reviewed");
        return;
    }

    let storage = tempfile::tempdir().unwrap();
    let root = storage.path().join("state");
    let store = StateStore::open(&root).unwrap();
    // Preinitialize the directories and lock so the child tests publication.
    drop(store.lock().unwrap());
    let output = Command::new("sh")
        .args(["-c", "umask 0200; exec \"$@\"", "sh"])
        .arg(env::current_exe().unwrap())
        .args([
            "--exact",
            "restrictive_umask_keeps_new_records_and_objects_readable",
        ])
        .env(CHILD_ROOT, &root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
