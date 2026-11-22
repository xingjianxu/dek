use std::{fs, path::Path, sync::Barrier, thread};

use dek_controller::repo::{
    EntryLanguage, HostSelector, InputPolicy, RepoError, Repository, Target, initialize,
};
use serde_json::{Value, json};
use tempfile::tempdir;

fn write_manifest(root: &Path, manifest: Value) {
    fs::write(
        root.join("dek.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
}

#[test]
fn manifest_exposes_entries_relative_to_the_repository_root() {
    let dir = tempdir().unwrap();
    write_manifest(
        dir.path(),
        json!({
            "schemaVersion": 1,
            "hosts": {
                "server": {
                    "entry": "hosts/server/default.rb",
                    "target": { "ssh": "lab-server" },
                    "inputs": "sdk"
                }
            },
            "profiles": {
                "base": "profiles/base.json",
                "tracked": { "entry": "profiles/tracked.rb", "inputs": "sdk" }
            },
            "modules": {
                "lab.backup": {
                    "schema": "modules/lab-backup/options.json",
                    "expand": "modules/lab-backup/expand.rb",
                    "readsOptions": ["lab.backup"],
                    "inputs": "sdk"
                },
                "legacy": {
                    "schema": "modules/legacy/options.json",
                    "expand": "modules/legacy/expand.rb"
                }
            }
        }),
    );
    let start = dir.path().join("nested/folder");
    fs::create_dir_all(&start).unwrap();
    let repo = Repository::discover(&start, None).unwrap();
    let root = dir.path().canonicalize().unwrap();
    let (_, host) = repo.select_host(HostSelector::Named("server")).unwrap();
    assert_eq!(host.target, Target::Ssh("lab-server".into()));
    assert_eq!(host.inputs, Some(InputPolicy::Sdk));
    assert_eq!(
        repo.root().join(&host.entry),
        root.join("hosts/server/default.rb")
    );

    let manifest = repo.manifest();
    assert_eq!(
        manifest.profiles["base"].entry,
        Path::new("profiles/base.json")
    );
    assert_eq!(manifest.profiles["base"].inputs, None);
    assert_eq!(
        manifest.profiles["tracked"].entry,
        Path::new("profiles/tracked.rb")
    );
    assert_eq!(manifest.profiles["tracked"].inputs, Some(InputPolicy::Sdk));
    let module = &manifest.modules["lab.backup"];
    assert_eq!(
        repo.root().join(&module.schema),
        root.join("modules/lab-backup/options.json")
    );
    assert_eq!(
        repo.root().join(&module.expand),
        root.join("modules/lab-backup/expand.rb")
    );
    assert_eq!(
        module.reads_options.as_deref(),
        Some(&["lab.backup".to_owned()][..])
    );
    assert_eq!(module.inputs, Some(InputPolicy::Sdk));
    assert_eq!(manifest.modules["legacy"].reads_options, None);
}

#[test]
fn local_selection_requires_unique_local_hostname_match_and_lists_candidates() {
    let dir = tempdir().unwrap();
    let mut manifest = json!({
        "schemaVersion": 1,
        "hosts": {
            "node": { "entry": "node.json", "target": { "local": true } },
            "NODE": { "entry": "server.rb", "target": { "ssh": "lab-server" } }
        }
    });
    write_manifest(dir.path(), manifest.clone());
    let repo = Repository::load(dir.path()).unwrap();
    let (name, host) = repo.select_host(HostSelector::Local("NODE")).unwrap();
    assert_eq!(name, "node");
    assert_eq!(host.target, Target::Local);

    for (selector, reason) in [
        (HostSelector::Named("missing"), "未知主机"),
        (HostSelector::Local("missing"), "没有匹配"),
    ] {
        let error = repo.select_host(selector).unwrap_err().to_string();
        for text in [reason, "missing", "可选主机", "node", "NODE"] {
            assert!(error.contains(text), "{error}");
        }
    }

    manifest["hosts"]["NODE"]["target"] = json!({ "local": true });
    write_manifest(dir.path(), manifest);
    let repo = Repository::load(dir.path()).unwrap();
    let error = repo
        .select_host(HostSelector::Local("node"))
        .unwrap_err()
        .to_string();
    for text in ["多条", "可选主机", "node", "NODE"] {
        assert!(error.contains(text), "{error}");
    }
    assert_eq!(
        repo.select_host(HostSelector::Named("NODE")).unwrap().0,
        "NODE"
    );
}

#[test]
fn rejects_invalid_manifest_fields_with_source_path() {
    let cases = [
        ("/schemaVersion", json!(2), "schemaVersion"),
        ("/hosts/node/entry", json!("../outside.json"), "entry"),
        ("/hosts/node/target", json!({ "local": false }), "target"),
        ("/hosts/node/target", json!({ "ssh": "" }), "target"),
        (
            "/hosts/node/target",
            json!({ "ssh": "lab-server", "local": true }),
            "target",
        ),
        ("/hosts/node/inputs", json!("unexpected"), "inputs"),
        ("/profiles/base", json!(true), "profiles"),
        ("/modules/lab.backup/schema", json!(""), "schema"),
        ("/modules/lab.backup/expand", json!(false), "expand"),
        (
            "/modules/lab.backup/readsOptions",
            json!(["lab..backup"]),
            "readsOptions",
        ),
    ];
    let dir = tempdir().unwrap();
    for (pointer, bad_value, expected) in cases {
        let mut manifest = json!({
            "schemaVersion": 1,
            "hosts": {
                "node": { "entry": "node.json", "target": { "local": true }, "inputs": "sdk" }
            },
            "profiles": { "base": "profiles/base.json" },
            "modules": {
                "lab.backup": {
                    "schema": "modules/backup/options.json",
                    "expand": "modules/backup/expand.rb",
                    "readsOptions": ["lab.backup"]
                }
            }
        });
        *manifest.pointer_mut(pointer).unwrap() = bad_value;
        write_manifest(dir.path(), manifest);
        let error = Repository::load(dir.path()).unwrap_err().to_string();
        assert!(
            error.contains("dek.json") && error.contains(expected),
            "{pointer}: {error}"
        );
    }
}

#[test]
fn diagnoses_a_future_version_before_decoding_v1_fields() {
    let dir = tempdir().unwrap();
    write_manifest(
        dir.path(),
        json!({ "schemaVersion": 2, "machines": ["later-version"] }),
    );
    let error = Repository::load(dir.path()).unwrap_err().to_string();
    assert!(
        error.contains("dek.json") && error.contains("schemaVersion 2"),
        "{error}"
    );
}

#[test]
fn rejects_positional_arrays_in_place_of_manifest_objects() {
    let dir = tempdir().unwrap();
    for (manifest, field) in [
        (json!([1, {}]), "object"),
        (
            json!({ "schemaVersion": 1, "hosts": { "node": ["node.json", { "local": true }] } }),
            "hosts",
        ),
        (
            json!({ "schemaVersion": 1, "hosts": {}, "profiles": { "base": ["profiles/base.rb", "sdk"] } }),
            "profiles",
        ),
        (
            json!({ "schemaVersion": 1, "hosts": {}, "modules": { "mod": ["options.json", "expand.rb", [], "sdk"] } }),
            "modules",
        ),
    ] {
        write_manifest(dir.path(), manifest.clone());
        let error = Repository::load(dir.path()).unwrap_err().to_string();
        assert!(
            error.contains("dek.json") && error.contains(field),
            "{manifest}: {error}"
        );
    }
}

#[test]
fn duplicate_host_keys_cannot_replace_an_earlier_binding() {
    let dir = tempdir().unwrap();
    fs::write(
        dir.path().join("dek.json"),
        r#"{"schemaVersion":1,"hosts":{
            "server":{"entry":"server.json","target":{"local":true}},
            "server":{"entry":"server.rb","target":{"ssh":"lab-server"}}
        }}"#,
    )
    .unwrap();
    let error = Repository::load(dir.path()).unwrap_err().to_string();
    for text in ["dek.json", "hosts", "server", "重复"] {
        assert!(error.contains(text), "{error}");
    }
}

#[test]
fn malformed_inner_manifest_is_not_replaced_by_a_valid_parent() {
    let dir = tempdir().unwrap();
    write_manifest(dir.path(), json!({ "schemaVersion": 1, "hosts": {} }));
    let inner = dir.path().join("inner");
    let start = inner.join("nested");
    fs::create_dir_all(&start).unwrap();
    fs::write(inner.join("dek.json"), "{ malformed }").unwrap();

    let error = Repository::discover(&start, None).unwrap_err().to_string();
    assert!(
        error.contains(&inner.join("dek.json").to_string_lossy().to_string()),
        "{error}"
    );
    let error = Repository::discover(&start, Some(Path::new(".")))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains(&start.join("dek.json").to_string_lossy().to_string()),
        "{error}"
    );
}

#[test]
fn concurrent_initializations_leave_only_the_winning_entry() {
    let dir = tempdir().unwrap();
    let gate = Barrier::new(2);
    let attempt = |language| {
        gate.wait();
        initialize(dir.path(), "node", language)
    };
    let results = thread::scope(|scope| {
        let json = scope.spawn(|| attempt(EntryLanguage::Json));
        let ruby = attempt(EntryLanguage::Ruby);
        (json.join().unwrap(), ruby)
    });
    let rejected = match results {
        (Ok(()), Err(rejected)) | (Err(rejected), Ok(())) => rejected,
        results => panic!("expected one successful initialization, got {results:?}"),
    };
    assert!(matches!(rejected, RepoError::InitConflict(_)), "{rejected}");

    let repo = Repository::load(dir.path()).unwrap();
    let entry = repo.root().join(&repo.manifest().hosts["node"].entry);
    let files: Vec<_> = fs::read_dir(entry.parent().unwrap())
        .unwrap()
        .map(|file| file.unwrap().path())
        .collect();
    assert_eq!(files, vec![entry]);
}

#[cfg(unix)]
#[test]
fn initialization_refuses_symlinked_entry_directories() {
    use std::os::unix::fs::symlink;

    let outside = tempdir().unwrap();
    for relative in ["hosts", "hosts/node"] {
        let dir = tempdir().unwrap();
        let link = dir.path().join(relative);
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(outside.path(), &link).unwrap();

        let error = initialize(dir.path(), "node", EntryLanguage::Json)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(&link.to_string_lossy().to_string()),
            "{error}"
        );
        assert!(!dir.path().join("dek.json").exists());
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }
}
