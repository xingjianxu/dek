#![cfg(target_os = "linux")]

use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::Path,
    process::{Command, Output},
};

use serde_json::{Value, json};

struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("dek.json"),
            r#"{"schemaVersion":1,"hosts":{"fixture":{"target":{"local":true},"entry":"host.json"}}}"#,
        )
        .unwrap();
        Self { root }
    }

    fn set_directories(&self, rules: Value) {
        fs::write(
            self.root.path().join("host.json"),
            serde_json::to_vec(&json!({
                "schemaVersion": 1,
                "imports": [],
                "options": [
                    {"path": "systemd.tmpfiles.settings.fixture", "value": rules},
                ],
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn command(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_dek"))
            .arg("--repo")
            .arg(self.root.path())
            .args(["-H", "fixture", args[0], "--state-dir"])
            .arg(self.root.path().join("state"))
            .args(&args[1..])
            .env("XDG_CACHE_HOME", self.root.path().join("cache"))
            .output()
            .unwrap()
    }

    fn plan(&self) -> Value {
        let output = self.command(&["plan", "--json"]);
        require_success(&output);
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

fn require_success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().mode() & 0o7777
}

#[test]
fn creates_parent_before_child_repairs_only_declared_directory_metadata_and_stops_on_omission() {
    let fixture = Fixture::new();
    let parent = fixture.root.path().join("dir with spaces");
    let child = parent.join("nested");
    fixture.set_directories(json!({
        (child.to_str().unwrap()): {"d": {"mode": "0750"}},
        (parent.to_str().unwrap()): {"d": {"mode": "0700"}},
    }));
    let plan = fixture.plan();
    let actions = plan["actions"].as_array().unwrap();
    assert_eq!(actions.len(), 2);
    assert_eq!(
        actions[0]["kind"]["ensureDirectory"]["path"],
        parent.to_str().unwrap()
    );
    assert_eq!(
        actions[1]["kind"]["ensureDirectory"]["path"],
        child.to_str().unwrap()
    );
    assert_eq!(actions[1]["dependsOn"][0], actions[0]["id"]);
    assert!(!parent.exists());
    require_success(&fixture.command(&["apply", "--plan", plan["plan_id"].as_str().unwrap()]));
    assert_eq!(mode(&parent), 0o700);
    assert_eq!(mode(&child), 0o750);
    assert_eq!(fixture.plan()["actions"], json!([]));

    let file = child.join("data");
    fs::write(&file, b"retained").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
    fs::set_permissions(&child, fs::Permissions::from_mode(0o777)).unwrap();
    let before = fs::metadata(&child).unwrap();
    let plan = fixture.plan();
    assert_eq!(plan["actions"].as_array().unwrap().len(), 1);
    require_success(&fixture.command(&["apply", "--plan", plan["plan_id"].as_str().unwrap()]));
    let after = fs::metadata(&child).unwrap();
    assert_eq!(
        (after.ino(), after.uid(), after.gid()),
        (before.ino(), before.uid(), before.gid())
    );
    assert_eq!(mode(&child), 0o750);
    assert_eq!(mode(&file), 0o640);
    assert_eq!(fs::read(&file).unwrap(), b"retained");

    fixture.set_directories(json!({}));
    fs::set_permissions(&child, fs::Permissions::from_mode(0o731)).unwrap();
    require_success(&fixture.command(&["apply"]));
    assert_eq!(mode(&child), 0o731);
    assert_eq!(fs::read(&file).unwrap(), b"retained");
}

#[test]
fn rejects_files_and_links_in_final_or_ancestor_components() {
    let fixture = Fixture::new();
    let real_directory = fixture.root.path().join("real");
    let existing_file = fixture.root.path().join("file");
    let link = fixture.root.path().join("link");
    fs::create_dir(&real_directory).unwrap();
    fs::write(&existing_file, b"untouched").unwrap();
    symlink(&real_directory, &link).unwrap();
    for target in [&existing_file, &link, &link.join("nested")] {
        fixture.set_directories(json!({ (target.to_str().unwrap()): {"d": {"mode": "0750"}} }));
        let output = fixture.command(&["plan", "--json"]);
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "unexpected plan for {target:?}");
        assert!(error.contains("目录路径类型冲突"), "{error}");
        assert!(error.contains(target.to_str().unwrap()), "{error}");
    }
    assert_eq!(fs::read(&existing_file).unwrap(), b"untouched");
    assert!(!real_directory.join("nested").exists());
}

#[test]
fn a_replaced_directory_requires_a_fresh_plan_even_with_the_same_metadata() {
    let fixture = Fixture::new();
    let path = fixture.root.path().join("managed");
    let saved = fixture.root.path().join("original");
    fs::create_dir(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o777)).unwrap();
    fixture.set_directories(json!({ (path.to_str().unwrap()): {"d": {"mode": "0750"}} }));
    let approved = fixture.plan();

    fs::rename(&path, &saved).unwrap();
    fs::create_dir(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o777)).unwrap();
    let result = fixture.command(&["apply", "--plan", approved["plan_id"].as_str().unwrap()]);
    assert!(!result.status.success());
    assert_eq!(mode(&saved), 0o777);
    assert_eq!(mode(&path), 0o777);
    let fresh = fixture.plan();
    assert_ne!(fresh["plan_id"], approved["plan_id"]);

    require_success(&fixture.command(&["apply", "--plan", fresh["plan_id"].as_str().unwrap()]));
    assert_eq!(mode(&saved), 0o777);
    assert_eq!(mode(&path), 0o750);
}

#[test]
fn an_owner_can_repair_a_directory_with_no_read_permission() {
    let fixture = Fixture::new();
    let path = fixture.root.path().join("managed");
    fs::create_dir(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    fixture.set_directories(json!({ (path.to_str().unwrap()): {"d": {"mode": "0700"}} }));
    require_success(&fixture.command(&["apply"]));
    assert_eq!(mode(&path), 0o700);
}
