#![cfg(target_os = "linux")]

use std::{ffi::OsStr, fs, path::Path, process::Command};

#[test]
fn benchmark_json_is_opt_in_and_preserves_the_cli_result() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    fs::write(
        root.join("dek.json"),
        r#"{"schemaVersion":1,"hosts":{"host":{"entry":"host.json","target":{"local":true}}}}"#,
    )
    .unwrap();
    fs::write(
        root.join("host.json"),
        r#"{"schemaVersion":1,"imports":[],"options":[]}"#,
    )
    .unwrap();

    let run = |encoding: &str, cache: &Path| {
        let output = Command::new(env!("CARGO_BIN_EXE_dek"))
            .args(["--repo"])
            .arg(root)
            .args(["-H", "host", "plan", "--json", "--state-dir"])
            .arg(root.join("state"))
            .env("XDG_CACHE_HOME", cache)
            .env("DEK_BENCH_CACHE_ENCODING", encoding)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    };

    let standard = root.join("standard");
    let benchmark = root.join("benchmark");
    let expected = run("msgpack", &standard);
    assert_eq!(run("json", &benchmark), expected);
    let extension = if cfg!(feature = "benchmark-cache") {
        "json"
    } else {
        "mpk"
    };
    assert!(contains_record(&standard, "mpk"));
    assert!(contains_record(&benchmark, extension));
    assert_eq!(run("json", &benchmark), expected);
}

fn contains_record(path: &Path, extension: &str) -> bool {
    fs::read_dir(path).unwrap().any(|entry| {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            contains_record(&entry.path(), extension)
        } else {
            entry.path().extension() == Some(OsStr::new(extension))
        }
    })
}
