#![cfg(target_os = "linux")]

use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Command, Output},
};

use dek_controller::{
    cache::{CacheEntry, UnitKey},
    modules::ModuleOutput,
};
use dek_core::{
    AgeSecretResource, ContentDigest, DesiredResource, FileAttributes, FileResource, FileState,
    HostConfig, Resource, ResourceId, ResourceOrigin, ResourceSet, UnitSpec,
};
use dek_linux::secrets::{SecretBackend, verification_marker};
use dek_target::planner::plan as plan_resources;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const IDENTITY: &[u8] = b"fixture identity bytes 82c74ed6";
const PLAINTEXT: &[u8] = b"fixture plaintext bytes a90673e4";
const CIPHERTEXT: &[u8] = b"age-encryption.org/v1\nfixture ciphertext 2bfa12e8";

fn command(root: &Path, host: &str, json: bool) -> Output {
    let cli =
        std::env::var_os("DEK_AGE_TEST_CLI").unwrap_or_else(|| env!("CARGO_BIN_EXE_dek").into());
    let mut command = Command::new(cli);
    command
        .arg("--repo")
        .arg(root)
        .args(["--host", host, "plan", "--state-dir"])
        .arg(root.join("state"))
        .env("XDG_CACHE_HOME", root.join("cache"));
    if json {
        command.arg("--json");
    }
    command.output().unwrap()
}

fn configure(root: &Path, host: &str, output: &Path, identity: &Path, mode: &str) {
    fs::write(
        root.join(format!("{host}.json")),
        serde_json::to_vec(&json!({
            "schemaVersion": 1,
            "imports": [],
            "options": [
                {"path": "age.identityPaths", "value": [identity]},
                {"path": "age.secrets.deploy.file", "value": "deploy.age"},
                {"path": "age.secrets.deploy.path", "value": output},
                {"path": "age.secrets.deploy.mode", "value": mode}
            ]
        }))
        .unwrap(),
    )
    .unwrap();
}

fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::write(
        root.join("dek.json"),
        r#"{"schemaVersion":1,"hosts":{"a":{"target":{"local":true},"entry":"a.json"},"b":{"target":{"local":true},"entry":"b.json"}}}"#,
    )
    .unwrap();
    fs::write(root.join("deploy.age"), CIPHERTEXT).unwrap();
    let output = root.join("output");
    let identity_a = root.join("a.agekey");
    let identity_b = root.join("b.agekey");
    fs::write(&output, PLAINTEXT).unwrap();
    fs::set_permissions(&output, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&identity_a, IDENTITY).unwrap();
    fs::write(&identity_b, IDENTITY).unwrap();
    (temp, output, identity_a, identity_b)
}

fn plan(root: &Path, host: &str) -> Value {
    let result = command(root, host, true);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    serde_json::from_slice(&result.stdout).unwrap()
}

#[test]
fn cli_persists_ciphertext_and_reports_unverified_state_for_both_identities() {
    let (temp, output, identity_a, identity_b) = fixture();
    let root = temp.path();
    configure(root, "a", &output, &identity_a, "0600");
    configure(root, "b", &output, &identity_b, "0600");

    let digest = format!("sha256:{:x}", Sha256::digest(CIPHERTEXT));
    for (host, identity) in [("a", identity_a), ("b", identity_b)] {
        let plan = plan(root, host);
        let action = &plan["actions"][0];
        assert_eq!(action["kind"]["deployAgeSecret"]["ciphertext"], digest);
        assert_eq!(
            action["kind"]["deployAgeSecret"]["identityPaths"][0],
            identity.to_str().unwrap()
        );
        assert!(action["reason"].as_str().unwrap().contains("应用时核对"));
        let id = plan["plan_id"].as_str().unwrap();
        let stored: Value =
            rmp_serde::from_slice(&fs::read(root.join(format!("state/plans/{id}.mpk"))).unwrap())
                .unwrap();
        assert_eq!(stored["plan"]["actions"], plan["actions"]);
        assert_no_secret(&serde_json::to_vec(&stored).unwrap());
        assert_no_secret(&serde_json::to_vec(&plan).unwrap());
    }
    let text = command(root, "a", false);
    assert!(text.status.success());
    assert!(String::from_utf8_lossy(&text.stdout).contains(&digest));
    assert_no_secret(&text.stdout);
    assert_eq!(
        fs::read(
            root.join("state/objects")
                .join(digest.strip_prefix("sha256:").unwrap())
        )
        .unwrap(),
        CIPHERTEXT
    );
    assert!(scan_cache(&root.join("cache")) > 0);
    assert_eq!(fs::read(output).unwrap(), PLAINTEXT);
}

#[test]
fn protected_marker_distinguishes_matching_drift_mode_and_new_ciphertext() {
    let (temp, output, identity, _) = fixture();
    let root = temp.path();
    configure(root, "a", &output, &identity, "0600");
    plan(root, "a");
    let secrets = root.join("state/secrets");
    fs::create_dir(&secrets).unwrap();
    fs::set_permissions(&secrets, fs::Permissions::from_mode(0o700)).unwrap();
    let secret = resource(&output, &identity);
    let key = [42; 32];
    let (id, marker) = verification_marker(&secret, &key, PLAINTEXT).unwrap();
    let marker_path = secrets.join(format!("{id}.marker"));
    let key_path = secrets.join("verification.key");
    private_file(&key_path, &key);
    private_file(&marker_path, &marker);
    assert!(plan(root, "a")["actions"].as_array().unwrap().is_empty());

    fs::set_permissions(&output, fs::Permissions::from_mode(0o644)).unwrap();
    let attrs = plan(root, "a");
    assert!(attrs["actions"][0]["kind"]["setAgeSecretAttributes"].is_object());
    assert_eq!(attrs["actions"][0]["reason"], "权限需修正");
    fs::set_permissions(&output, fs::Permissions::from_mode(0o600)).unwrap();

    fs::write(&output, b"changed local bytes").unwrap();
    let drift = plan(root, "a");
    assert_eq!(drift["actions"][0]["reason"], "已部署内容发生漂移");
    fs::write(&output, PLAINTEXT).unwrap();

    fs::write(root.join("deploy.age"), b"alternate reviewed ciphertext").unwrap();
    let changed = plan(root, "a");
    assert!(
        changed["actions"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("密文摘要")
    );
    fs::write(root.join("deploy.age"), CIPHERTEXT).unwrap();
    private_file(&key_path, &[43; 32]);
    let rotated = plan(root, "a");
    assert!(
        rotated["actions"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("应用时核对")
    );
    assert_no_secret(&serde_json::to_vec(&drift).unwrap());
    assert_eq!(fs::read(&output).unwrap(), PLAINTEXT);
    fs::set_permissions(&marker_path, fs::Permissions::from_mode(0o644)).unwrap();
    let exposed = command(root, "a", true);
    assert!(!exposed.status.success());
    assert!(String::from_utf8_lossy(&exposed.stderr).contains("0600"));
    assert_no_secret(&exposed.stderr);
}

#[test]
fn injected_registry_keeps_the_advertised_file_only_capability() {
    let (temp, _, _, _) = fixture();
    let mut result = Vec::new();
    dek_cli::command::run(
        [
            "dek",
            "--repo",
            temp.path().to_str().unwrap(),
            "-H",
            "a",
            "target-info",
            "--json",
        ],
        &mut result,
        || dek_cli::local_target::local_registry(&dek_cli::local_target::LocalFileBackend),
    )
    .unwrap();
    let capabilities: Value = serde_json::from_slice(&result).unwrap();
    assert_eq!(capabilities["resources"].as_array().unwrap().len(), 1);
    assert_eq!(capabilities["resources"][0]["kind"], "file");
}

#[test]
fn plans_reject_direct_symlink_and_hard_link_identity_collisions() {
    let (temp, output, identity, _) = fixture();
    let root = temp.path();
    let linked = root.join("symlink");
    let hard_linked = root.join("hardlink");
    let parent_alias = root.join("parent-alias");
    let broken = root.join("broken");
    symlink(&identity, &linked).unwrap();
    symlink(root, &parent_alias).unwrap();
    symlink(root.join("missing"), &broken).unwrap();
    fs::hard_link(&identity, &hard_linked).unwrap();

    for path in [
        &identity,
        &linked,
        &hard_linked,
        &parent_alias.join("a.agekey"),
        &broken,
    ] {
        configure(root, "a", path, &identity, "0600");
        let result = command(root, "a", true);
        assert!(!result.status.success());
        let diagnostic = String::from_utf8_lossy(&result.stderr);
        assert!(diagnostic.contains("a.json"), "{diagnostic}");
        assert!(
            diagnostic.contains("identity") || diagnostic.contains("符号链接"),
            "{diagnostic}"
        );
        assert_no_secret(&result.stderr);
    }
    assert_eq!(fs::read(identity).unwrap(), IDENTITY);
    assert_eq!(fs::read(output).unwrap(), PLAINTEXT);
}

#[test]
fn plan_protects_identities_from_other_resources_and_allows_a_missing_parent() {
    let (temp, output, identity, _) = fixture();
    let root = temp.path();
    let missing_parent = root.join("new").join("directory").join("output");
    configure(root, "a", &missing_parent, &identity, "0600");
    assert_eq!(plan(root, "a")["actions"][0]["reason"], "目标加密文件缺失");

    let alias = root.join("ordinary-file");
    fs::hard_link(&identity, &alias).unwrap();
    let backend = SecretBackend::new(root.join("state"));
    let mut registry =
        dek_cli::local_target::local_registry(&dek_cli::local_target::LocalFileBackend).unwrap();
    registry.register(&backend).unwrap();
    let origin = ResourceOrigin {
        module: "age fixture".into(),
        origin: dek_core::Origin::Json {
            file: "age.json".into(),
            pointer: "/options/4".parse().unwrap(),
        },
    };
    let resources = ResourceSet {
        resources: vec![
            Resource {
                desired: DesiredResource::File(FileResource {
                    path: alias.to_str().unwrap().into(),
                    state: FileState::Absent,
                }),
                origins: vec![],
                depends_on: vec![],
                notifications: vec![],
            },
            Resource {
                desired: DesiredResource::AgeSecret(resource(&output, &identity)),
                origins: vec![origin],
                depends_on: vec![ResourceId::Path(alias.to_str().unwrap().into())],
                notifications: vec![],
            },
        ],
    };
    let error = plan_resources(&registry, "repo", &resources, &[])
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("age.json") && error.contains("identity"),
        "{error}"
    );
    assert_no_secret(error.as_bytes());

    fs::remove_file(&alias).unwrap();
    fs::hard_link(&output, &alias).unwrap();
    let Err(error) = plan_resources(&registry, "repo", &resources, &[]) else {
        panic!("ordinary file resources must not expose a managed secret through a hard link");
    };
    let error = error.to_string();
    assert!(
        error.contains("age.json") && error.contains("加密文件落点"),
        "{error}"
    );
    assert_no_secret(error.as_bytes());
}

fn resource(output: &Path, identity: &Path) -> AgeSecretResource {
    AgeSecretResource {
        path: output.to_str().unwrap().into(),
        ciphertext: ContentDigest(format!("sha256:{:x}", Sha256::digest(CIPHERTEXT))),
        identity_paths: vec![identity.to_str().unwrap().into()],
        attributes: FileAttributes {
            mode: Some("0600".into()),
            ..FileAttributes::default()
        },
    }
}

fn private_file(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn scan_cache(path: &Path) -> usize {
    let mut count = 0;
    for entry in fs::read_dir(path).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            count += scan_cache(&path);
        } else if path.extension().is_some_and(|extension| extension == "mpk") {
            let encoded = fs::read(path).unwrap();
            assert_no_secret(&encoded);
            type CacheRecord<T> = (u32, UnitKey, String, CacheEntry<T>);
            let decoded = if let Ok(record) =
                rmp_serde::from_slice::<CacheRecord<UnitSpec>>(&encoded)
            {
                serde_json::to_vec(&record).unwrap()
            } else if let Ok(record) = rmp_serde::from_slice::<CacheRecord<HostConfig>>(&encoded) {
                serde_json::to_vec(&record).unwrap()
            } else {
                let record: CacheRecord<ModuleOutput> = rmp_serde::from_slice(&encoded).unwrap();
                serde_json::to_vec(&record).unwrap()
            };
            assert_no_secret(&decoded);
            count += 1;
        }
    }
    count
}

fn assert_no_secret(bytes: &[u8]) {
    for secret in [IDENTITY, PLAINTEXT] {
        let digest = Sha256::digest(secret);
        let hex = format!("{digest:x}");
        for needle in [secret, digest.as_ref(), hex.as_bytes()] {
            assert!(!bytes.windows(needle.len()).any(|window| window == needle));
        }
    }
}
