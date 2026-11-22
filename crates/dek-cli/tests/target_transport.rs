#![cfg(unix)]

use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Output, Stdio},
};

use serde_json::{Value, json};
use tempfile::tempdir;

fn write_manifest(root: &Path, target_id: Option<&str>) {
    let mut host = json!({"entry": "server.json", "target": {"ssh": "lab-alias"}});
    if let Some(id) = target_id {
        host["targetId"] = json!(id);
    }
    fs::write(
        root.join("dek.json"),
        json!({"schemaVersion": 1, "hosts": {"server": host}}).to_string(),
    )
    .unwrap();
}

fn target_info(root: &Path, script: &str) -> Output {
    let shim = root.join("ssh");
    fs::write(&shim, format!("#!/bin/sh\n{script}\n")).unwrap();
    fs::set_permissions(shim, fs::Permissions::from_mode(0o700)).unwrap();
    run_target_info(root)
}

fn run_target_info(root: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dek"))
        .args(["target-info", "-H", "server", "--json", "--repo"])
        .arg(root)
        .env("PATH", root)
        .env("DEK_TEST_SSH_ARGS", root.join("args"))
        .output()
        .unwrap()
}

fn hello(id: &str, version: u32) -> String {
    json!({"hello": {
        "capabilities": {
            "target": {"id": id, "os": "linux", "distribution": "arch", "architecture": "x86_64"},
            "protocolVersion": version,
            "resources": [{"kind": "file", "version": 1, "attributes": ["mode"]}]
        }
    }})
    .to_string()
}

fn fake_server(response: &str) -> String {
    format!(
        "printf '%s\\n' \"$@\" > \"$DEK_TEST_SSH_ARGS\"\n\
         IFS= read -r request\n\
         printf '%s\\n' '{response}'\n\
         IFS= read -r unexpected && exit 51\n\
         exit 0"
    )
}

#[test]
fn manifest_binding_is_independent_of_the_ssh_alias() {
    let dir = tempdir().unwrap();
    write_manifest(dir.path(), None);
    let output = target_info(dir.path(), &fake_server(&hello("server", 1)));
    assert!(output.status.success(), "{:?}", output);
    let capabilities: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(capabilities["protocolVersion"], 1);
    assert_eq!(capabilities["target"]["id"], "server");
    assert_eq!(capabilities["resources"][0]["kind"], "file");
    let arguments = fs::read_to_string(dir.path().join("args")).unwrap();
    assert_eq!(
        arguments.lines().collect::<Vec<_>>(),
        [
            "-T",
            "-S",
            "none",
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "--",
            "lab-alias",
            "dek __target-stdio"
        ]
    );

    write_manifest(dir.path(), Some("expected-target"));
    let output = target_info(dir.path(), &fake_server(&hello("expected-target", 1)));
    assert!(output.status.success(), "{:?}", output);
    let output = target_info(dir.path(), &fake_server(&hello("trusted-other", 1)));
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error = String::from_utf8_lossy(&output.stderr);
    for part in ["lab-alias", "expected-target", "trusted-other"] {
        assert!(error.contains(part), "{error}");
    }
}

#[test]
fn target_info_distinguishes_connection_binary_and_protocol_failures() {
    let dir = tempdir().unwrap();
    write_manifest(dir.path(), None);
    for (script, expected) in [
        ("IFS= read -r hello; exit 255".into(), "OpenSSH 连接失败"),
        (
            "IFS= read -r hello; exit 127".into(),
            "目标机找不到 dek 可执行文件",
        ),
        (fake_server(&hello("server", 99)), "99"),
        (fake_server(r#"{"incompatible":{"versions":[98]}}"#), "98"),
        (fake_server("not-json"), "目标协议数据无效"),
        (fake_server(r#"{"hello":"#), "目标协议数据无效"),
    ] {
        let output = target_info(dir.path(), &script);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains("lab-alias") && error.contains(expected),
            "{error}"
        );
    }

    fs::remove_file(dir.path().join("ssh")).unwrap();
    let output = run_target_info(dir.path());
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("lab-alias") && error.contains("无法启动 OpenSSH"),
        "{error}"
    );
}

#[test]
fn hidden_target_endpoint_round_trips_the_original_path_and_requires_hello() {
    let directory = tempdir().unwrap();
    let path = directory
        .path()
        .join("name with 'single' and \"double\" quotes");
    let operation = json!({"request": {"id": 9, "operation": {"normalizePath": {"path": path}}}});
    let mut process = Command::new(env!("CARGO_BIN_EXE_dek"));
    process
        .arg("__target-stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = process.spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    writeln!(input, "{}", json!({"hello": {"versions": [1]}})).unwrap();
    writeln!(input, "{operation}").unwrap();
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{:?}", output);
    let replies = String::from_utf8(output.stdout).unwrap();
    let replies: Vec<Value> = replies
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[0]["hello"]["capabilities"]["protocolVersion"], 1);
    assert_eq!(replies[1]["response"]["id"], 9);
    assert_eq!(
        replies[1]["response"]["result"]["normalizedPath"]["original"],
        json!(path)
    );

    let mut child = process.spawn().unwrap();
    writeln!(child.stdin.take().unwrap(), "{operation}").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
}

#[test]
#[ignore = "requires two provisioned Arch guests; run scripts/tests/ssh-handshake"]
fn quoted_path_round_trips_over_real_ssh() {
    use dek_controller::transport::SshSession;
    use dek_core::target_protocol::{TargetRequest, TargetResult};

    let id = std::env::var("DEK_TEST_ARCH_TARGET_ID").unwrap();
    let mut session = SshSession::connect("dek-a", &id).unwrap();
    for path in [
        "/tmp/dek file with 'single' and \"double\" quotes",
        "/tmp/dek file with a newline\nand a backslash\\",
    ] {
        let reply = session
            .request(TargetRequest::NormalizePath { path: path.into() })
            .unwrap();
        assert_eq!(
            reply,
            TargetResult::NormalizedPath {
                original: path.into(),
                normalized: path.into(),
            }
        );
    }
    let invalid = session
        .request(TargetRequest::NormalizePath {
            path: "relative".into(),
        })
        .unwrap();
    assert!(matches!(invalid, TargetResult::Error { .. }));
    session.finish().unwrap();
}
