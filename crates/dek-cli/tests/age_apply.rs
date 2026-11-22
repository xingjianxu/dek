#![cfg(target_os = "linux")]

use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Command, Output},
};

use age::secrecy::ExposeSecret;
use serde_json::{Value, json};

const PLAINTEXT: &[u8] = b"dek temporary fixture private bytes 117b32de\n";

struct Fixture {
    root: tempfile::TempDir,
    target: PathBuf,
    identity: PathBuf,
    signer: age::x25519::Identity,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("destination");
        let identity = root.path().join("fixture.agekey");
        let signer = age::x25519::Identity::generate();
        fs::write(
            root.path().join("dek.json"),
            r#"{"schemaVersion":1,"hosts":{"local":{"target":{"local":true},"entry":"host.json"}}}"#,
        )
        .unwrap();
        let fixture = Self {
            root,
            target,
            identity,
            signer,
        };
        fixture.write_identity(&fixture.signer);
        fixture.configure("0600", &fixture.target);
        fixture.encrypt(PLAINTEXT, &fixture.signer);
        fixture
    }

    fn write_identity(&self, signer: &age::x25519::Identity) {
        fs::write(&self.identity, signer.to_string().expose_secret()).unwrap();
        fs::set_permissions(&self.identity, fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn configure(&self, mode: &str, path: &Path) {
        let config = json!({
            "schemaVersion": 1,
            "imports": [],
            "options": [
                {"path":"age.identityPaths","value":[self.identity]},
                {"path":"age.secrets.test.file","value":"secret.age"},
                {"path":"age.secrets.test.path","value":path},
                {"path":"age.secrets.test.mode","value":mode},
            ],
        });
        fs::write(self.root.path().join("host.json"), config.to_string()).unwrap();
    }

    fn encrypt(&self, bytes: &[u8], signer: &age::x25519::Identity) {
        let encrypted = age::encrypt(&signer.to_public(), bytes).unwrap();
        fs::write(self.root.path().join("secret.age"), encrypted).unwrap();
    }

    fn command(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_dek"))
            .arg("--repo")
            .arg(self.root.path())
            .args(["-H", "local", args[0], "--state-dir"])
            .arg(self.root.path().join("state"))
            .args(&args[1..])
            .env("XDG_CACHE_HOME", self.root.path().join("cache"))
            .output()
            .unwrap()
    }

    fn plan(&self) -> Value {
        let result = self.command(&["plan", "--json"]);
        assert_success(&result);
        serde_json::from_slice(&result.stdout).unwrap()
    }

    fn apply(&self) -> Output {
        let plan = self.plan();
        self.command(&["apply", "--plan", plan["plan_id"].as_str().unwrap()])
    }
}

fn assert_success(result: &Output) {
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

fn no_secret(bytes: &[u8]) {
    assert!(
        !bytes
            .windows(PLAINTEXT.len())
            .any(|value| value == PLAINTEXT)
    );
}

#[test]
fn deploys_and_reconciles_without_replacing_equal_plaintext() {
    let fixture = Fixture::new();
    let result = fixture.apply();
    assert_success(&result);
    no_secret(&result.stdout);
    no_secret(&result.stderr);
    assert_eq!(fs::read(&fixture.target).unwrap(), PLAINTEXT);
    assert_eq!(
        fs::metadata(&fixture.target).unwrap().mode() & 0o7777,
        0o600
    );
    assert_eq!(fixture.plan()["actions"], json!([]));

    let inode = fs::metadata(&fixture.target).unwrap().ino();
    let another = age::x25519::Identity::generate();
    fixture.write_identity(&another);
    fixture.encrypt(PLAINTEXT, &another);
    assert_success(&fixture.apply());
    assert_eq!(fs::metadata(&fixture.target).unwrap().ino(), inode);
    assert_eq!(fixture.plan()["actions"], json!([]));

    fs::remove_file(&fixture.identity).unwrap();
    fs::set_permissions(&fixture.target, fs::Permissions::from_mode(0o640)).unwrap();
    let plan = fixture.plan();
    assert!(
        plan["actions"][0]["kind"]
            .get("setAgeSecretAttributes")
            .is_some()
    );
    assert_success(&fixture.apply());
    assert_eq!(fs::metadata(&fixture.target).unwrap().ino(), inode);
    assert_eq!(
        fs::metadata(&fixture.target).unwrap().mode() & 0o7777,
        0o600
    );

    fixture.write_identity(&another);
    fs::write(&fixture.target, b"external replacement").unwrap();
    assert!(
        fixture.plan()["actions"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("漂移")
    );
    assert_success(&fixture.apply());
    assert_eq!(fs::read(&fixture.target).unwrap(), PLAINTEXT);
    assert_eq!(fixture.plan()["actions"], json!([]));
}

#[test]
fn rejects_corrupt_ciphertext_wrong_identity_and_link_collisions_without_changing_output() {
    let fixture = Fixture::new();
    assert_success(&fixture.apply());
    let inode = fs::metadata(&fixture.target).unwrap().ino();
    for bytes in [
        b"invalid encrypted object".to_vec(),
        age::encrypt(&age::x25519::Identity::generate().to_public(), b"other").unwrap(),
    ] {
        fs::write(fixture.root.path().join("secret.age"), bytes).unwrap();
        let result = fixture.apply();
        assert!(!result.status.success());
        assert_eq!(fs::read(&fixture.target).unwrap(), PLAINTEXT);
        assert_eq!(fs::metadata(&fixture.target).unwrap().ino(), inode);
        no_secret(&result.stdout);
        no_secret(&result.stderr);
    }
    let link = fixture.root.path().join("linked");
    symlink(&fixture.target, &link).unwrap();
    for output in [&link, &fixture.identity] {
        fixture.configure("0600", output);
        assert!(!fixture.command(&["plan", "--json"]).status.success());
    }
    assert!(!fs::read_dir(fixture.root.path()).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".dek-stage-")
    }));
}

#[test]
fn a_publication_checkpoint_certifies_only_the_complete_published_bytes() {
    let fixture = Fixture::new();
    assert_success(&fixture.apply());
    let state = fixture.root.path().join("state/secrets");
    let marker = fs::read_dir(&state)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "marker"))
        .unwrap();
    assert_eq!(
        fs::metadata(state.join("verification.key")).unwrap().mode() & 0o7777,
        0o600
    );
    let checkpoint = marker.with_extension("candidate");
    fs::rename(&marker, &checkpoint).unwrap();
    assert_eq!(fixture.plan()["actions"], json!([]));

    fs::write(&fixture.target, b"incomplete content").unwrap();
    assert_eq!(fixture.plan()["actions"].as_array().unwrap().len(), 1);
    assert_success(&fixture.apply());
    assert!(!checkpoint.exists());
    assert_eq!(fixture.plan()["actions"], json!([]));
}
