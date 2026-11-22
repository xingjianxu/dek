use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde_json::{Value, json};

fn dek(directory: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dek"));
    command.current_dir(directory);
    command
}

fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn write(root: &Path, path: &str, content: &str) {
    let file = root.join(path);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(file, content).unwrap();
}

#[test]
fn show_text_and_json_report_the_same_value_and_sources_without_ruby() {
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/minimal");
    let empty_path = tempfile::tempdir().unwrap();
    let text = success(
        dek(&repo)
            .env("PATH", empty_path.path())
            .args(["show", "services.caddy.enable", "-H", "server"])
            .output()
            .unwrap(),
    );
    for expected in [
        "services.caddy.enable = true",
        "hosts/server/default.json",
        "profiles/base.json",
    ] {
        assert!(text.contains(expected), "{text}");
    }
    let output = success(
        dek(&repo)
            .env("PATH", empty_path.path())
            .args([
                "show",
                "services.caddy.enable",
                "--host",
                "server",
                "--json",
            ])
            .output()
            .unwrap(),
    );
    let document: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(document["host"], "server");
    let options = document["options"].as_object().unwrap();
    assert_eq!(options.len(), 1);
    let option = &options["services.caddy.enable"];
    assert_eq!(option["value"], true);
    assert_eq!(option["priority"], "host");
    assert_eq!(
        option["effectiveDefinitions"][0]["origin"],
        json!({"file":"hosts/server/default.json","pointer":"/options/0"})
    );
    assert!(
        option["overriddenDefinitions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|definition| {
                definition["value"] == false && definition["origin"]["file"] == "profiles/base.json"
            })
    );
}

#[test]
fn show_uses_mixed_entries_custom_schema_and_explicit_repository() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "dek.json",
        &json!({
            "schemaVersion":1,
            "hosts":{"server":{"entry":"hosts/server.rb","target":{"ssh":"lab-server"}}},
            "profiles":{"base":"profiles/base.json"},
            "modules":{"lab.example":{"schema":"modules/options.json","expand":"modules/absent.rb"}}
        })
        .to_string(),
    );
    write(
        root,
        "hosts/server.rb",
        "profile 'base'\nuser 'alice', isNormalUser: true, shell: '/bin/bash'\n",
    );
    write(
        root,
        "profiles/base.json",
        &json!({
            "schemaVersion":1,
            "imports":[],
            "options":[{"path":"lab.example.enable","value":true}]
        })
        .to_string(),
    );
    write(root, "modules/options.json", &json!({
        "options":[{"path":"lab.example.enable","type":"boolean","description":"Enable example"}]
    }).to_string());
    write(root, "nested/dek.json", "invalid inner manifest");

    let output = success(
        dek(&root.join("nested"))
            .args(["show", "--json", "--repo", "..", "-H", "server"])
            .output()
            .unwrap(),
    );
    let shown: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(shown["target"], json!({"ssh":"lab-server"}));
    assert_eq!(
        shown["options"]["users.users.alice.isNormalUser"]["value"],
        true
    );
    assert_eq!(
        shown["options"]["users.users.alice.shell"]["value"],
        "/bin/bash"
    );
    assert_eq!(
        shown["options"]["users.users.alice.shell"]["effectiveDefinitions"][0]["origin"],
        json!({"file":"hosts/server.rb","line":2})
    );
    assert_eq!(shown["options"]["lab.example.enable"]["value"], true);

    let output = dek(root)
        .args([
            "show",
            "users.users.alice.unknown",
            "-H",
            "server",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains("users.users.alice.unknown"), "{error}");

    for (entry, reason) in [
        ("profile 'missing'\n", "missing"),
        (
            "user 'alice', isNormalUser: 'invalid'\n",
            "users.users.alice.isNormalUser",
        ),
        ("value = (\n", "syntax"),
    ] {
        write(root, "hosts/server.rb", entry);
        let output = dek(root)
            .args(["show", "-H", "server", "--json"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(
            error.contains("hosts/server.rb:1") && error.contains(reason),
            "{error}"
        );
    }
}

#[test]
fn show_matches_the_current_local_host_and_has_help() {
    let dir = tempfile::tempdir().unwrap();
    let help = success(dek(dir.path()).args(["show", "--help"]).output().unwrap());
    for expected in ["show", "--json", "--repo", "--host"] {
        assert!(help.contains(expected), "{help}");
    }
    success(dek(dir.path()).arg("init").output().unwrap());
    let output = success(dek(dir.path()).args(["show", "--json"]).output().unwrap());
    let shown: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(
        shown["host"],
        hostname::get().unwrap().into_string().unwrap()
    );
    assert_eq!(shown["target"], json!({"local":true}));
}
