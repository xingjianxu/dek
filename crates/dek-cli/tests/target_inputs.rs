#![cfg(unix)]

use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Output, Stdio},
};

use serde_json::{Value, json};

#[test]
fn target_endpoint_reports_readable_missing_and_unreadable_inputs() {
    let work = tempfile::tempdir().unwrap();
    let text = work.path().join("a file \"with quotes\"");
    let missing = work.path().join("missing");
    let invalid = work.path().join("binary");
    let oversized = work.path().join("oversized");
    fs::File::create(&oversized)
        .unwrap()
        .set_len(dek_core::target_protocol::MAX_FRAME_BYTES as u64)
        .unwrap();
    fs::write(&text, "目标文本\n").unwrap();
    fs::write(&invalid, [0xff]).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_dek"))
        .arg("__target-stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    writeln!(input, "{}", json!({"hello": {"versions": [1]}})).unwrap();
    let queries = [
        ("readText", text.to_str().unwrap()),
        ("readText", missing.to_str().unwrap()),
        ("readText", invalid.to_str().unwrap()),
        ("readText", "relative"),
        ("fact", "architecture"),
        ("fact", "unknown"),
        ("capability", "file"),
        ("capability", "unknown"),
        ("readText", oversized.to_str().unwrap()),
    ];
    for (index, (kind, key)) in queries.into_iter().enumerate() {
        writeln!(
            input,
            "{}",
            json!({"request": {"id": index + 1, "operation": {
                "queryInput": {"kind": kind, "key": key}
            }}})
        )
        .unwrap();
    }
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let results: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .skip(1)
        .map(|frame| frame["response"]["result"].clone())
        .collect();
    assert_eq!(results.len(), queries.len());
    assert_eq!(results[0]["input"]["value"], "目标文本\n");
    assert!(
        results[0]["input"]["observation"]["digest"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    assert_eq!(
        results[1]["input"],
        json!({"value": null, "observation": "missing"})
    );
    assert!(
        results[2]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("utf-8")
    );
    assert!(
        results[3]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("absolute target path")
    );
    assert_eq!(results[4]["input"]["value"], std::env::consts::ARCH);
    assert_eq!(results[5]["input"]["observation"], "missing");
    assert_eq!(results[6]["input"]["value"], true);
    assert_eq!(results[7]["input"]["value"], false);
    assert!(
        results[8]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("frame limit")
    );
}

fn show(root: &Path, host: &str, snapshot: bool) -> Output {
    let path = std::env::join_paths(
        std::iter::once(root.to_owned())
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_dek"));
    command
        .args(["show", "--json", "-H", host, "--repo"])
        .arg(root)
        .env("PATH", path)
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("DEK_TEST_PEER", root.join("peer.rb"))
        .env("DEK_TEST_ROOT", root);
    if snapshot {
        command.arg("--target-snapshot");
    }
    command.output().unwrap()
}

fn shown(output: Output) -> Value {
    assert!(output.status.success(), "{output:?}");
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn show_records_bound_target_inputs_and_rechecks_live_values() {
    let work = tempfile::tempdir().unwrap();
    let root = work.path();
    let ssh = root.join("ssh");
    fs::write(
        &ssh,
        "#!/bin/sh\nfor arg do alias=$command; command=$arg; done\nexec ruby \"$DEK_TEST_PEER\" \"$alias\"\n",
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(
        root.join("peer.rb"),
        r##"
require 'digest'
require 'json'
root = ENV.fetch('DEK_TEST_ROOT')
name = ARGV.fetch(0)
exit 255 if File.exist?(File.join(root, 'offline'))
JSON.parse(STDIN.gets)
puts JSON.generate(hello: {capabilities: {
  target: {id: "vm-#{name}", os: 'linux', architecture: 'x86_64'},
  protocolVersion: 1,
  resources: []
}})
STDOUT.flush
STDIN.each_line do |line|
  req = JSON.parse(line).fetch('request')
  query = req.fetch('operation').fetch('queryInput')
  value = case query.fetch('kind')
  when 'fact'
    "arch-#{name}" unless File.exist?(File.join(root, 'no-fact'))
  when 'readText'
    file = File.join(root, "#{name}-#{File.basename(query.fetch('key'))}")
    File.read(file) if File.exist?(file)
  when 'capability'
    false
  end
  encoded = query.fetch('kind') == 'readText' ? value : JSON.generate(value)
  digest = value.nil? ? 'missing' : {digest: "sha256:#{Digest::SHA256.hexdigest(encoded)}"}
  puts JSON.generate(response: {id: req.fetch('id'), result: {
    input: {value: value, observation: digest}
  }})
  STDOUT.flush
end
"##,
    )
    .unwrap();
    let manifest = |target_id| {
        fs::write(
            root.join("dek.json"),
            json!({"schemaVersion": 1, "hosts": {
                "alpha": {"entry": "host.rb", "inputs": "sdk", "target": {"ssh": "a"}, "targetId": target_id},
                "beta": {"entry": "host.rb", "inputs": "sdk", "target": {"ssh": "b"}, "targetId": "vm-b"}
            }}).to_string(),
        )
        .unwrap();
    };
    manifest("vm-a");
    fs::write(
        root.join("host.rb"),
        r#"configure 'programs.git.config', { 'facts' => {
  'local' => read_text('current'),
  'target' => target.read_text('/remote/current'),
  'optional' => target.read_text('/remote/optional') || 'missing',
  'architecture' => target.fact('architecture'),
  'systemd' => target.capability('systemd').to_s
} }"#,
    )
    .unwrap();
    fs::write(root.join("current"), "manager").unwrap();
    fs::write(root.join("a-current"), "on a").unwrap();
    fs::write(root.join("b-current"), "on b").unwrap();
    let facts =
        |document: &Value| document["options"]["programs.git.config"]["value"]["facts"].clone();

    let alpha = shown(show(root, "alpha", false));
    let beta = shown(show(root, "beta", false));
    assert!(alpha.get("targetSnapshot").is_none());
    assert_eq!(facts(&alpha)["local"], "manager");
    assert_eq!(facts(&alpha)["target"], "on a");
    assert_eq!(facts(&alpha)["architecture"], "arch-a");
    assert_eq!(facts(&alpha)["systemd"], "false");
    assert_eq!(facts(&alpha)["optional"], "missing");
    assert_eq!(facts(&beta)["target"], "on b");

    fs::write(root.join("offline"), "").unwrap();
    fs::write(root.join("a-current"), "changed a").unwrap();
    fs::write(root.join("a-optional"), "appeared").unwrap();
    for (name, document, binding) in [("alpha", &alpha, "vm-a"), ("beta", &beta, "vm-b")] {
        let snapshot = shown(show(root, name, true));
        assert_eq!(facts(&snapshot), facts(document));
        let label = snapshot["targetSnapshot"]["label"].as_str().unwrap();
        assert!(label.contains(name) && label.contains(binding), "{label}");
    }
    manifest("another-target");
    let never_observed = show(root, "alpha", true);
    assert!(!never_observed.status.success(), "{never_observed:?}");
    assert!(
        String::from_utf8_lossy(&never_observed.stderr).contains("another-target"),
        "{never_observed:?}"
    );
    manifest("vm-a");
    assert!(!show(root, "alpha", false).status.success());

    fs::remove_file(root.join("offline")).unwrap();
    let current = shown(show(root, "alpha", false));
    assert_eq!(facts(&current)["target"], "changed a");
    assert_eq!(facts(&current)["optional"], "appeared");

    fs::write(root.join("no-fact"), "").unwrap();
    let missing = show(root, "alpha", false);
    assert!(!missing.status.success());
    let diagnostic = String::from_utf8_lossy(&missing.stderr);
    for text in ["architecture", "host.rb:5", "vm-a"] {
        assert!(diagnostic.contains(text), "{diagnostic}");
    }
    assert!(!show(root, "alpha", true).status.success());
}
