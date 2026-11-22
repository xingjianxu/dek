use std::fs;

use dek_cli::local_target::{LocalFileBackend, local_registry};
use dek_core::{
    ActionKind, ContentDigest, DesiredResource, FileAttributes, FileContent, FileObservation,
    FileResource, FileState, Precondition, Resource, ResourceSet,
};
use dek_target::planner::plan;
use sha2::{Digest, Sha256};

fn digest(bytes: &[u8]) -> ContentDigest {
    ContentDigest(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn file(path: &str, content: &[u8], mode: Option<&str>) -> ResourceSet {
    ResourceSet {
        resources: vec![Resource {
            desired: DesiredResource::File(FileResource {
                path: path.into(),
                state: FileState::Present(FileContent {
                    digest: digest(content),
                    attributes: FileAttributes {
                        mode: mode.map(str::to_owned),
                        ..FileAttributes::default()
                    },
                }),
            }),
            origins: Vec::new(),
            depends_on: Vec::new(),
            notifications: Vec::new(),
        }],
    }
}

#[test]
fn live_local_file_planning_observes_drift_and_leaves_the_file_untouched() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("local.conf");
    let target = local_registry(&LocalFileBackend).unwrap();
    let spec = file(path.to_str().unwrap(), b"reviewed", None);

    let missing = plan(&target, "repo", &spec, &[]).unwrap();
    assert!(!path.exists());
    let [action] = &missing.actions[..] else {
        panic!("expected one write")
    };
    assert!(
        matches!(&action.kind, ActionKind::WriteFile { content, .. } if content.digest == digest(b"reviewed"))
    );
    assert!(matches!(
        &action.preconditions[..],
        [Precondition::File {
            expected: FileObservation::Missing,
            ..
        }]
    ));

    fs::write(&path, b"actual").unwrap();
    let changed = plan(&target, "repo", &spec, &[]).unwrap();
    assert_ne!(missing.id, changed.id);
    assert!(matches!(&changed.actions[0].preconditions[..], [
        Precondition::File { expected: FileObservation::Present(actual), .. }
    ] if actual.digest == digest(b"actual")));
    assert_eq!(fs::read(&path).unwrap(), b"actual");

    fs::write(&path, b"reviewed").unwrap();
    assert!(
        plan(&target, "repo", &spec, &[])
            .unwrap()
            .actions
            .is_empty()
    );
}

#[cfg(unix)]
#[test]
fn live_mode_drift_is_a_separate_action_and_symlinks_are_rejected() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let root = tempfile::tempdir().unwrap();
    let destination = root.path().join("config");
    fs::write(&destination, b"same").unwrap();
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o644)).unwrap();
    let target = local_registry(&LocalFileBackend).unwrap();
    let spec = file(destination.to_str().unwrap(), b"same", Some("0600"));
    let changed = plan(&target, "repo", &spec, &[]).unwrap();
    assert!(
        matches!(&changed.actions[..], [action] if matches!(&action.kind,
        ActionKind::SetFileAttributes { attributes, .. } if attributes.mode.as_deref() == Some("0600")))
    );
    assert_eq!(
        fs::metadata(&destination).unwrap().permissions().mode() & 0o7777,
        0o644
    );

    let link = root.path().join("symlink");
    symlink(&destination, &link).unwrap();
    let spec = file(link.to_str().unwrap(), b"same", None);
    let error = plan(&target, "repo", &spec, &[]).unwrap_err().to_string();
    assert!(
        error.contains("regular file") && error.contains("symlink"),
        "{error}"
    );
}

#[test]
fn text_and_json_expose_the_same_target_plan_and_dependency_ids() {
    use dek_cli::plan_output::write_plan;
    use serde_json::Value;

    let root = tempfile::tempdir().unwrap();
    let file_a = root.path().join("a");
    let file_b = root.path().join("b");
    let mut spec = file(file_a.to_str().unwrap(), b"one", None);
    let mut dependent = file(file_b.to_str().unwrap(), b"two", None)
        .resources
        .remove(0);
    dependent.depends_on.push(spec.resources[0].id());
    spec.resources.push(dependent);
    let target = local_registry(&LocalFileBackend).unwrap();
    let reviewed = plan(&target, "repo", &spec, &[]).unwrap();

    let mut encoded = Vec::new();
    write_plan(&mut encoded, "example", &reviewed, true).unwrap();
    let json: Value = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(json["host"], "example");
    assert_eq!(json["plan_id"], reviewed.id);
    assert_eq!(json["target"]["id"], reviewed.target.id);
    assert_eq!(
        json["actions"],
        serde_json::to_value(&reviewed.actions).unwrap()
    );

    let mut text = Vec::new();
    write_plan(&mut text, "example", &reviewed, false).unwrap();
    let text = String::from_utf8(text).unwrap();
    for part in [
        &reviewed.id,
        &reviewed.actions[0].reason,
        &reviewed.actions[0].id,
        &reviewed.target.id,
    ] {
        assert!(text.contains(part), "{text}");
    }
    assert!(text.contains("依赖") && text.contains("更新文件"), "{text}");
}

#[test]
fn local_ruby_queries_distinguish_missing_inputs_and_record_capability_observations() {
    use dek_cli::local_target::LocalEvaluationTarget;
    use dek_controller::eval::TargetProvider;
    use dek_core::{
        Observation,
        protocol::{TargetQuery, TargetQueryKind},
    };
    use serde_json::Value;

    let dir = tempfile::tempdir().unwrap();
    let target = local_registry(&LocalFileBackend).unwrap();
    let mut provider = LocalEvaluationTarget(&target);
    let mut query = TargetQuery {
        protocol_version: 1,
        parent_id: "collect".into(),
        id: "target".into(),
        kind: TargetQueryKind::ReadText,
        key: dir.path().join("optional.txt").to_str().unwrap().into(),
        origin: None,
    };
    let missing = provider.query(&query).unwrap();
    assert_eq!(missing.value, Value::Null);
    assert_eq!(missing.observation, Observation::Missing);

    fs::write(&query.key, "").unwrap();
    let empty = provider.query(&query).unwrap();
    assert_eq!(empty.value, "");
    assert_eq!(empty.observation, Observation::Digest(digest(b"").0));

    query.kind = TargetQueryKind::Capability;
    query.key = "file".into();
    let yes = provider.query(&query).unwrap();
    assert_eq!(yes.value, true);
    query.key = "file.owner".into();
    let no = provider.query(&query).unwrap();
    assert_eq!(no.value, false);
    assert_ne!(yes.observation, no.observation);

    query.kind = TargetQueryKind::Fact;
    query.key = "architecture".into();
    let observed = provider.query(&query).unwrap();
    assert_eq!(observed.value, target.target().architecture);
    query.key = "unavailable".into();
    assert_eq!(
        provider.query(&query).unwrap().observation,
        Observation::Missing
    );
}
