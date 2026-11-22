#![cfg(target_os = "linux")]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use serde_json::{Value, json};
use tempfile::TempDir;

struct Fixture {
    work: TempDir,
    target: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let work = tempfile::tempdir().unwrap();
        let root = work.path();
        let bin = root.join("bin");
        fs::create_dir(&bin).unwrap();
        fs::write(
            bin.join("ssh"),
            "#!/bin/sh\nexec \"$DEK_TEST_BINARY\" __target-stdio --state-dir \"$DEK_TEST_STATE\"\n",
        )
        .unwrap();
        fs::set_permissions(bin.join("ssh"), fs::Permissions::from_mode(0o700)).unwrap();
        let id = hostname::get().unwrap().into_string().unwrap();
        fs::write(
            root.join("dek.json"),
            json!({
                "schemaVersion": 1,
                "hosts": {
                    "alpha": {"entry": "host.json", "target": {"ssh": "test"}, "targetId": id},
                    "beta": {"entry": "host.json", "target": {"ssh": "test"}, "targetId": id},
                },
                "modules": {
                    "lab.files": {"schema": "options.json", "expand": "expand.rb", "inputs": "sdk"},
                }
            })
            .to_string(),
        )
        .unwrap();
        fs::write(
            root.join("host.json"),
            r#"{"schemaVersion":1,"imports":[],"options":[]}"#,
        )
        .unwrap();
        fs::write(
            root.join("options.json"),
            r#"{"options":[{"path":"lab.files.enable","type":"boolean","default":true,"description":"fixture"}]}"#,
        )
        .unwrap();
        let target = root.join("managed.txt");
        fs::write(
            root.join("expand.rb"),
            format!(
                "file_resource {}, content: 'reviewed'\n",
                json!(target.to_str().unwrap())
            ),
        )
        .unwrap();
        Self { work, target }
    }

    fn root(&self) -> &Path {
        self.work.path()
    }

    fn run(&self, host: &str, args: &[&str]) -> Output {
        let path = std::env::join_paths(
            std::iter::once(self.root().join("bin"))
                .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap();
        let binary = env!("CARGO_BIN_EXE_dek");
        Command::new(binary)
            .args(["-H", host, "--repo"])
            .arg(self.root())
            .args(args)
            .env("PATH", path)
            .env("XDG_CACHE_HOME", self.root().join("cache"))
            .env("DEK_TEST_BINARY", binary)
            .env("DEK_TEST_ROOT", self.root())
            .env("DEK_TEST_STATE", self.root().join("state"))
            .output()
            .unwrap()
    }
}

fn successful(output: Output) -> Vec<u8> {
    assert!(output.status.success(), "{output:?}");
    output.stdout
}

fn document(output: Output) -> Value {
    serde_json::from_slice(&successful(output)).unwrap()
}

#[test]
fn ssh_apply_replays_reviewed_content_and_exposes_bound_status() {
    let fixture = Fixture::new();
    assert_eq!(
        document(fixture.run("alpha", &["status", "--json"])),
        Value::Null
    );
    let plan = document(fixture.run("alpha", &["plan", "--json"]));
    let plan_id = plan["plan_id"].as_str().unwrap();
    fs::write(fixture.root().join("expand.rb"), "unused after review\n").unwrap();

    let apply = &["apply", "--plan", plan_id, "--request-id", "stable-token"];
    assert!(!fixture.run("beta", apply).status.success());
    assert!(!fixture.target.exists());
    let first = successful(fixture.run("alpha", apply));
    let first = String::from_utf8(first).unwrap();
    let run = first
        .lines()
        .find_map(|line| line.strip_prefix("run_id "))
        .unwrap();
    assert_eq!(fs::read(&fixture.target).unwrap(), b"reviewed");

    let status = document(fixture.run("alpha", &["status", "--json", "--run", run]));
    assert_eq!(status["request_id"], "stable-token");
    assert_eq!(status["plan_id"], plan_id);
    assert_eq!(status["run_id"], run);
    assert_eq!(status["finished"], true);
    assert_eq!(status["actions"][0]["attempt"], 1);
    assert_eq!(
        status,
        document(fixture.run("alpha", &["status", "--json"]))
    );
    assert_eq!(first.as_bytes(), successful(fixture.run("alpha", apply)));
    assert!(
        !fixture
            .run("beta", &["status", "--json", "--run", run])
            .status
            .success()
    );
}

#[test]
fn implicit_ssh_apply_displays_upgrade_without_executing_it() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root().join("bin/ssh"),
        "#!/bin/sh\nexec ruby \"$DEK_TEST_ROOT/upgrade-peer.rb\"\n",
    )
    .unwrap();
    let target_id = hostname::get().unwrap().into_string().unwrap();
    let target = json!({"id": target_id, "os": "linux", "architecture": "x86_64"});
    let plan = json!({
        "formatVersion": 1, "id": "review-upgrade", "target": target,
        "resourceDigest": "sha256:fixture", "usedCapabilities": [], "usedInputs": [],
        "actions": [{"id": "upgrade", "kind": {"repositoryTransaction": {
            "scope": "upgrade", "targets": [], "changes": []
        }}, "reason": "review required", "origins": [], "dependsOn": [], "preconditions": []}]
    });
    let trace = fixture.root().join("upgrade-requests");
    fs::write(
        fixture.root().join("upgrade-peer.rb"),
        format!(
            r#"
require 'json'
STDIN.gets
plan = JSON.parse({encoded:?})
puts JSON.generate(hello: {{capabilities: {{target: plan.fetch('target'), protocolVersion: 1, resources: []}}}})
STDOUT.flush
STDIN.each_line do |line|
  request = JSON.parse(line).fetch('request')
  operation = request.fetch('operation')
  name = operation.is_a?(Hash) ? operation.keys.fetch(0) : operation
  File.open({trace:?}, 'a') {{ |log| log.puts name }}
  reply = case name
  when 'runStatus'
    {{executionStatus: nil}}
  when 'preparePlan'
    plan['repositoryId'] = operation.fetch(name).fetch('request').fetch('repositoryId')
    {{missingContents: {{digests: []}}}}
  when 'finishPlan'
    {{planned: {{plan: plan, pendingNotifications: []}}}}
  else
    abort "unexpected operation #{{name}}"
  end
  puts JSON.generate(response: {{id: request.fetch('id'), result: reply}})
  STDOUT.flush
end
"#,
            encoded = plan.to_string(),
            trace = trace.to_str().unwrap(),
        ),
    )
    .unwrap();
    let output = fixture.run("alpha", &["apply"]);
    assert!(!output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("完整仓库升级事务"), "{stdout}");
    assert!(
        stdout.contains("dek apply -H alpha --plan review-upgrade"),
        "{stdout}"
    );
    assert_eq!(
        fs::read_to_string(trace).unwrap(),
        "runStatus\npreparePlan\nfinishPlan\n"
    );
}
