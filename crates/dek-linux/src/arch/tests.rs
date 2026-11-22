use std::{fs, os::unix::fs::PermissionsExt};

use dek_core::{
    Action, ActionKind, DesiredResource, PackageId, PackageResource, PackageSource, Precondition,
    Presence,
};
use dek_target::backend::{ActionReadiness, Backend};

use super::{PacmanBackend, package_rows};

fn request(name: &str, state: Presence) -> DesiredResource {
    DesiredResource::Package(PackageResource {
        package: PackageId {
            source: PackageSource::Official,
            name: name.into(),
        },
        state,
    })
}

fn fixture(preview: &str) -> (tempfile::TempDir, PacmanBackend) {
    let root = tempfile::tempdir().unwrap();
    let program = root.path().join("pacman");
    fs::write(
        &program,
        format!(
            r#"#!/bin/sh
state_dir=$(dirname "$0")
printf '%s\n' "$*" >> "$state_dir/argv"
case "$1:${{2:-}}" in
    --query:--color) printf 'ripgrep 15.2.0-1\ntree 2.3.2-1\n' ;;
    --remove:--print) printf '%s\n' '{}' ;;
    *) printf 'hook diagnostic\n'; printf 'unexpected invocation\n' >&2; exit 49 ;;
esac
"#,
            preview
        ),
    )
    .unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    (root, PacmanBackend { program })
}

#[test]
fn observes_local_presence_and_reports_a_missing_package_without_sync() {
    let (fixture, backend) = fixture("tree\t2.3.2-1");
    let installed = request("ripgrep", Presence::Present);
    let observed = backend.observe(&installed).unwrap();
    assert!(
        matches!(&observed[..], [Precondition::PackageVersion { expected: Some(version), .. }] if version == "15.2.0-1")
    );
    assert!(backend.plan(&installed, &observed).unwrap().is_empty());
    let missing = request("git", Presence::Present);
    assert!(matches!(
        &backend.observe(&missing).unwrap()[..],
        [Precondition::PackageVersion { expected: None, .. }]
    ));
    assert!(
        backend
            .plan(&missing, &backend.observe(&missing).unwrap())
            .unwrap_err()
            .to_string()
            .contains("git")
    );
    assert_eq!(
        fs::read_to_string(fixture.path().join("argv")).unwrap(),
        "--query --color never\n".repeat(3)
    );
}

#[test]
fn only_previews_the_reviewed_single_package_and_version() {
    let (_single, backend) = fixture("tree\t2.3.2-1");
    let desired = request("tree", Presence::Absent);
    let conditions = backend.observe(&desired).unwrap();
    let planned = backend.plan(&desired, &conditions).unwrap().pop().unwrap();
    assert!(
        matches!(&planned.kind, ActionKind::RemovePackage { package } if package.name == "tree")
    );
    assert_eq!(planned.preconditions, conditions);
    let action = Action {
        id: "remove".into(),
        kind: planned.kind,
        reason: planned.reason,
        origins: Vec::new(),
        depends_on: Vec::new(),
        preconditions: planned.preconditions,
    };
    assert_eq!(
        backend.inspect_action(&action).unwrap(),
        ActionReadiness::Ready
    );
    let (_broad, broad) = fixture("tree\t2.3.2-1\nneovim\t0.11-1");
    assert!(broad.plan(&desired, &conditions).is_err());
    let (_changed, changed) = fixture("tree\t2.3.1-1");
    assert!(changed.plan(&desired, &conditions).is_err());
}

#[test]
fn rejects_ambiguous_rows_and_unavailable_sources() {
    assert!(package_rows("tree 2.3.2-1\ntree 2.3.1-1\n").is_err());
    assert!(package_rows("tree\n").is_err());
    let backend = PacmanBackend::default();
    let aur = DesiredResource::Package(PackageResource {
        package: PackageId {
            source: PackageSource::Aur,
            name: "tree".into(),
        },
        state: Presence::Present,
    });
    assert_ne!(aur.id(), request("tree", Presence::Present).id());
    assert!(
        backend
            .validate(&aur)
            .unwrap_err()
            .to_string()
            .contains("AUR")
    );
    assert!(
        backend
            .validate(&request("--nodeps", Presence::Absent))
            .is_err()
    );
}

#[test]
fn reports_native_hook_details_from_both_output_streams() {
    let (_fixture, backend) = fixture("tree\t2.3.2-1");
    let error = backend
        .output("test", &["--unexpected"])
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("hook diagnostic") && error.contains("unexpected invocation"),
        "{error}"
    );
}
