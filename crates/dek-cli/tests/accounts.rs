#![cfg(target_os = "linux")]

use std::{fs, process::Command};

use serde_json::{Value, json};

#[test]
fn plans_local_groups_before_users_without_allocating_ids() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path();
    fs::write(
        repo.join("dek.json"),
        r#"{"schemaVersion":1,"hosts":{"fixture":{"target":{"local":true},"entry":"host.json"}}}"#,
    )
    .unwrap();
    fs::write(
        repo.join("host.json"),
        serde_json::to_vec(&json!({
            "schemaVersion": 1,
            "imports": [],
            "options": [
                {"path":"users.groups.dek_acct_fixture","value":{}},
                {"path":"users.users.dek_acct_fixture.isNormalUser","value":true},
                {"path":"users.users.dek_acct_fixture.group","value":"dek_acct_fixture"}
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_dek"))
        .arg("--repo")
        .arg(repo)
        .args(["-H", "fixture", "plan", "--json", "--state-dir"])
        .arg(repo.join("state"))
        .env("XDG_CACHE_HOME", repo.join("cache"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    let actions = plan["actions"].as_array().unwrap();
    assert_eq!(actions.len(), 2);
    assert_eq!(
        actions[0]["kind"]["ensureGroup"]["name"],
        "dek_acct_fixture"
    );
    assert!(actions[0]["kind"]["ensureGroup"]["gid"].is_null());
    assert_eq!(actions[1]["kind"]["ensureUser"]["name"], "dek_acct_fixture");
    assert!(actions[1]["kind"]["ensureUser"]["uid"].is_null());
    assert_eq!(actions[1]["dependsOn"][0], actions[0]["id"]);
}
