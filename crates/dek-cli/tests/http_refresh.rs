use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

use serde_json::{Value, json};

#[path = "../../dek-controller/tests/support/http_server.rs"]
mod http_server;

use http_server::Server;

fn run(root: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dek"))
        .arg("--repo")
        .arg(root)
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("NO_PROXY", "127.0.0.1")
        .env("no_proxy", "127.0.0.1")
        .args(arguments)
        .output()
        .unwrap()
}

fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn plan(root: &Path) -> Value {
    serde_json::from_str(&success(run(root, &["plan", "-H", "server", "--json"]))).unwrap()
}

fn repository(root: &Path, server: &Server) {
    fs::write(
        root.join("dek.json"),
        json!({
            "schemaVersion": 1,
            "hosts": {
                "server": {"entry": "server.rb", "target": {"local": true}, "inputs": "sdk"},
                "other": {"entry": "other.rb", "target": {"local": true}, "inputs": "sdk"}
            },
            "modules": {"lab.payload": {"schema": "options.json", "expand": "expand.rb"}}
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        root.join("options.json"),
        json!({"options": [
            {"path": "lab.payload.enable", "type": "boolean", "default": false, "description": "启用"},
            {"path": "lab.payload.text", "type": "string", "description": "内容"}
        ]}).to_string(),
    ).unwrap();
    fs::write(
        root.join("expand.rb"),
        format!(
            r#"file_resource {:?}, content: option("lab.payload.text")"#,
            root.join("target.txt").to_str().unwrap()
        ),
    )
    .unwrap();

    for name in ["server", "other"] {
        let counter = root.join(format!("{name}.calls"));
        fs::write(
            root.join(format!("{name}.rb")),
            format!(
                "File.open({:?}, 'a') {{ |file| file.write('.') }}\nconfigure 'lab.payload', enable: true, text: http_text({:?})\n",
                counter.to_str().unwrap(), server.url(&format!("/{name}"))
            ),
        ).unwrap();
    }
}

#[test]
fn refreshing_one_host_updates_only_its_inputs_and_keeps_offline_plans_available() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let server = Server::start();
    let url = server.url("/server");
    server.respond("/server", 200, "original");
    server.respond("/other", 200, "unrelated");
    repository(root, &server);

    let cold = run(root, &["plan", "-H", "server", "--json"]);
    assert!(!cold.status.success());
    assert!(cold.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&cold.stderr);
    assert!(
        stderr.contains(&url) && stderr.contains("dek refresh"),
        "{stderr}"
    );
    assert_eq!(server.count("/server"), 0);
    assert_eq!(server.count("/other"), 0);

    success(run(root, &["refresh", "-H", "server"]));
    let initial = plan(root);
    assert_eq!(initial["actions"].as_array().unwrap().len(), 1);
    assert_eq!(server.count("/server"), 1);

    server.respond("/server", 200, "updated");
    success(run(root, &["refresh", "-H", "server"]));
    let before = fs::read(root.join("server.calls")).unwrap();
    let updated = plan(root);
    assert_ne!(initial["plan_id"], updated["plan_id"]);
    assert_eq!(
        fs::read(root.join("server.calls")).unwrap().len(),
        before.len() + 1
    );

    success(run(root, &["refresh", "-H", "server"]));
    let before = fs::read(root.join("server.calls")).unwrap();
    assert_eq!(plan(root), updated);
    assert_eq!(fs::read(root.join("server.calls")).unwrap(), before);
    assert_eq!(server.count("/server"), 3);
    assert_eq!(server.count("/other"), 0);
    assert!(!root.join("other.calls").exists());

    server.respond("/server", 503, "failure");
    let failed = run(root, &["refresh", "-H", "server"]);
    assert!(!failed.status.success());
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(stderr.contains(&url) && stderr.contains("503"), "{stderr}");
    assert_eq!(server.count("/server"), 4);
    drop(server);

    assert_eq!(plan(root), updated);
    let show = success(run(
        root,
        &["show", "lab.payload.text", "-H", "server", "--json"],
    ));
    assert_eq!(
        serde_json::from_str::<Value>(&show).unwrap()["options"]["lab.payload.text"]["value"],
        "updated"
    );
    assert!(!root.join("target.txt").exists());
    success(run(root, &["refresh", "--help"]));
}

#[test]
fn refresh_discovers_an_enabled_modules_current_http_urls() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let server = Server::start();
    repository(root, &server);
    let manifest_file = root.join("dek.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_file).unwrap()).unwrap();
    manifest["modules"]["lab.payload"]["inputs"] = json!("sdk");
    fs::write(manifest_file, manifest.to_string()).unwrap();
    fs::write(
        root.join("expand.rb"),
        format!(
            r#"file_resource {:?}, content: http_text(option("lab.payload.text"))"#,
            root.join("target.txt").to_str().unwrap()
        ),
    )
    .unwrap();

    let write_host = |path| {
        fs::write(
            root.join("server.rb"),
            format!(
                "configure 'lab.payload', enable: true, text: {:?}\n",
                server.url(path)
            ),
        )
        .unwrap();
    };
    server.respond("/old", 200, "from old URL");
    write_host("/old");
    success(run(root, &["refresh", "-H", "server"]));
    let old = plan(root);
    assert_eq!(server.count("/old"), 1);

    server.respond("/old", 503, "retired");
    server.respond("/current", 200, "from current URL");
    write_host("/current");
    success(run(root, &["refresh", "-H", "server"]));
    let current = plan(root);
    assert_ne!(old["plan_id"], current["plan_id"]);
    assert_eq!(server.count("/old"), 1);
    assert_eq!(server.count("/current"), 1);
    assert_eq!(server.count("/other"), 0);
}
