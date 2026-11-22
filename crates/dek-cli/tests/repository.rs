use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

use serde_json::{Value, json};
use tempfile::tempdir;

fn dek(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dek"))
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap()
}

fn success(output: Output) -> Vec<u8> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn failure(output: Output) -> String {
    assert!(
        !output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    String::from_utf8(output.stderr).unwrap()
}

fn hostname() -> String {
    hostname::get().unwrap().into_string().unwrap()
}

#[test]
fn init_creates_a_local_json_entry_in_the_current_directory() {
    let dir = tempdir().unwrap();
    success(dek(dir.path(), &["init"]));

    let name = hostname();
    let entry = format!("hosts/{name}/default.json");
    let listed: Value =
        serde_json::from_slice(&success(dek(dir.path(), &["hosts", "--json"]))).unwrap();
    assert_eq!(
        listed,
        json!([{ "name": name, "entry": entry, "target": { "local": true } }])
    );
    let contents: Value =
        serde_json::from_slice(&fs::read(dir.path().join(entry)).unwrap()).unwrap();
    assert_eq!(
        contents,
        json!({ "schemaVersion": 1, "imports": [], "options": [] })
    );
}

#[test]
fn init_creates_a_ruby_entry_in_an_explicit_directory() {
    let dir = tempdir().unwrap();
    success(dek(dir.path(), &["init", "machines dir", "--lang", "ruby"]));

    let repo = dir.path().join("machines dir");
    let name = hostname();
    let entry = format!("hosts/{name}/default.rb");
    let manifest: Value =
        serde_json::from_slice(&fs::read(repo.join("dek.json")).unwrap()).unwrap();
    assert_eq!(
        manifest["hosts"][name],
        json!({ "entry": entry, "inputs": "sdk", "target": { "local": true } })
    );
    assert!(!fs::read_to_string(repo.join(entry)).unwrap().is_empty());
}

#[test]
fn init_reports_the_existing_manifest_and_preserves_both_files() {
    let dir = tempdir().unwrap();
    success(dek(dir.path(), &["init"]));
    let manifest = dir.path().join("dek.json");
    let entry = dir
        .path()
        .join(format!("hosts/{}/default.json", hostname()));
    fs::write(&entry, "existing entry").unwrap();
    let original_manifest = fs::read(&manifest).unwrap();

    let error = failure(dek(dir.path(), &["init"]));
    assert!(error.contains("dek.json"), "{error}");
    assert_eq!(fs::read(manifest).unwrap(), original_manifest);
    assert_eq!(fs::read_to_string(entry).unwrap(), "existing entry");
}

#[test]
fn init_reports_an_existing_entry_before_creating_the_manifest() {
    let dir = tempdir().unwrap();
    let entry = dir
        .path()
        .join(format!("hosts/{}/default.json", hostname()));
    fs::create_dir_all(entry.parent().unwrap()).unwrap();
    fs::write(&entry, "existing entry").unwrap();

    let error = failure(dek(dir.path(), &["init"]));
    assert!(error.contains("default.json"), "{error}");
    assert_eq!(fs::read_to_string(entry).unwrap(), "existing entry");
    assert!(!dir.path().join("dek.json").exists());
}

#[test]
fn hosts_discovers_the_nearest_manifest_and_accepts_an_explicit_root() {
    let outer = tempdir().unwrap();
    fs::write(
        outer.path().join("dek.json"),
        r#"{"schemaVersion":1,"hosts":{
            "laptop":{"entry":"hosts/laptop/default.json","target":{"local":true}},
            "server":{"entry":"hosts/server/default.rb","target":{"ssh":"lab-server"}}
        }}"#,
    )
    .unwrap();
    let nested = outer.path().join("nested");
    let deep = nested.join("deep");
    fs::create_dir_all(&deep).unwrap();
    fs::write(
        nested.join("dek.json"),
        r#"{"schemaVersion":1,"hosts":{
            "inner":{"entry":"hosts/inner/default.json","target":{"local":true}}
        }}"#,
    )
    .unwrap();

    let listed = String::from_utf8(success(dek(&deep, &["hosts"]))).unwrap();
    assert!(listed.contains("inner") && listed.contains("hosts/inner/default.json"));
    assert!(!listed.contains("server"));

    let listed = String::from_utf8(success(dek(&deep, &["--repo", "../..", "hosts"]))).unwrap();
    assert!(listed.contains("laptop") && listed.contains("server"));
    assert!(!listed.contains("inner"));

    let selected: Value = serde_json::from_slice(&success(dek(
        &deep,
        &["hosts", "--json", "--repo", "../..", "-H", "server"],
    )))
    .unwrap();
    assert_eq!(
        selected,
        json!([{"name":"server","entry":"hosts/server/default.rb","target":{"ssh":"lab-server"}}])
    );

    let error = failure(dek(
        &deep,
        &["hosts", "--host", "unknown", "--repo", "../.."],
    ));
    assert!(
        ["unknown", "laptop", "server"]
            .iter()
            .all(|name| error.contains(name)),
        "{error}"
    );
}

#[test]
fn both_repository_commands_have_help() {
    let dir = tempdir().unwrap();
    for (command, flag) in [("init", "--lang"), ("hosts", "--json")] {
        let help = String::from_utf8(success(dek(dir.path(), &[command, "--help"]))).unwrap();
        assert!(help.contains(command) && help.contains(flag), "{help}");
    }
}
