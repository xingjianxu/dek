#![cfg(target_os = "linux")]

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

use dek_core::{
    Plan, TargetCapabilities,
    target_protocol::{CONTENT_CHUNK_BYTES, TargetRequest},
};
use dek_target::state::{PlanBinding, StateStore};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

fn digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

#[test]
fn cli_reuses_duplicate_content_and_reobserves_target_drift() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    let state = root.join("state");
    let target_id = hostname::get().unwrap().into_string().unwrap();
    let path_a = root.join("file-a");
    let path_b = root.join("file-b");
    let path_c = root.join("file-c");
    fs::write(
        root.join("dek.json"),
        json!({
            "schemaVersion":1,
            "hosts":{"server":{"entry":"host.json","target":{"ssh":"fake-arch"},"targetId":target_id}},
            "modules":{"lab.file":{"schema":"options.json","expand":"expand.rb","inputs":"sdk"}}
        }).to_string(),
    ).unwrap();
    fs::write(
        root.join("host.json"),
        r#"{"schemaVersion":1,"imports":[],"options":[]}"#,
    )
    .unwrap();
    fs::write(root.join("options.json"), r#"{"options":[{"path":"lab.file.enable","type":"boolean","default":true,"description":"file probe"}]}"#).unwrap();
    fs::write(
        root.join("expand.rb"),
        format!(
            "file_resource {path_a:?}, content: 'shared'\nfile_resource {path_b:?}, content: 'shared'\nfile_resource {path_c:?}, content: 'prior'\n"
        ),
    ).unwrap();
    let shim = root.join("ssh");
    fs::write(
        &shim,
        "#!/bin/sh\ntee \"$DEK_TEST_WIRE\" | \"$DEK_TEST_BIN\" __target-stdio --state-dir \"$DEK_TEST_STATE\"\n",
    ).unwrap();
    fs::set_permissions(shim, fs::Permissions::from_mode(0o700)).unwrap();

    let store = StateStore::open(&state).unwrap();
    store
        .lock()
        .unwrap()
        .put_object(&dek_core::ContentDigest(digest(b"prior")), b"prior")
        .unwrap();
    for (path, bytes) in [(&path_a, "shared"), (&path_b, "shared"), (&path_c, "prior")] {
        fs::write(path, bytes).unwrap();
    }
    let mut path = vec![root.to_path_buf()];
    path.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let run = || {
        let output = Command::new(env!("CARGO_BIN_EXE_dek"))
            .args(["plan", "-H", "server", "--json", "--repo"])
            .arg(root)
            .env("PATH", std::env::join_paths(&path).unwrap())
            .env("DEK_TEST_WIRE", root.join("wire"))
            .env("DEK_TEST_BIN", env!("CARGO_BIN_EXE_dek"))
            .env("DEK_TEST_STATE", &state)
            .env("XDG_CACHE_HOME", root.join("cache"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let content: Vec<_> = fs::read_to_string(root.join("wire"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter_map(|frame| frame.pointer("/request/operation/uploadContent").cloned())
            .collect();
        (
            serde_json::from_slice::<Value>(&output.stdout).unwrap(),
            content,
        )
    };

    let (fresh, uploads) = run();
    assert_eq!(fresh["actions"], json!([]));
    assert_eq!(
        uploads,
        [upload(&digest(b"shared"), "c2hhcmVk", true)["uploadContent"].clone()]
    );

    fs::write(&path_b, "manual").unwrap();
    let (drifted, uploads) = run();
    assert!(uploads.is_empty());
    assert_ne!(fresh["plan_id"], drifted["plan_id"]);
    let mut wire_plan = drifted.clone();
    let fields = wire_plan.as_object_mut().unwrap();
    fields.remove("host");
    let id = fields.remove("plan_id").unwrap();
    fields.insert("id".into(), id);
    let plan: Plan = serde_json::from_value(wire_plan).unwrap();
    assert_eq!(plan.target.id, target_id);
    assert_eq!(plan.actions.len(), 1);
    assert!(
        matches!(&plan.actions[0].kind, dek_core::ActionKind::WriteFile { path, .. } if Path::new(path) == path_b)
    );
    let review = store
        .lock()
        .unwrap()
        .load_plan(
            &plan.id,
            PlanBinding {
                repository_id: &plan.repository_id,
                logical_host: "server",
                target: &plan.target,
            },
        )
        .unwrap();
    assert_eq!(review.plan, plan);
    assert_eq!(fs::read(&path_b).unwrap(), b"manual");

    let output = root.join("cleartext");
    let ciphertext: Vec<_> = b"age-encryption.org/v1\n"
        .iter()
        .copied()
        .chain((0..=255).cycle())
        .take(CONTENT_CHUNK_BYTES + 7)
        .collect();
    fs::write(root.join("payload.age"), &ciphertext).unwrap();
    fs::write(
        root.join("host.json"),
        json!({"schemaVersion":1, "imports":[], "options":[
            {"path":"age.identityPaths", "value":[root.join("absent.agekey")]},
            {"path":"age.secrets.payload.file", "value":"payload.age"},
            {"path":"age.secrets.payload.path", "value":output}
        ]})
        .to_string(),
    )
    .unwrap();
    let (review, uploads) = run();
    let address = dek_core::ContentDigest(digest(&ciphertext));
    let mut transferred = Vec::new();
    let [first, last] = uploads.as_slice() else {
        panic!("expected two ciphertext chunks");
    };
    for (value, expected_complete) in [(first, false), (last, true)] {
        let TargetRequest::UploadContent {
            digest,
            chunk,
            complete,
        } = serde_json::from_value(json!({"uploadContent":value})).unwrap()
        else {
            panic!("expected ciphertext upload");
        };
        assert_eq!(digest, address);
        assert_eq!(complete, expected_complete);
        transferred.extend(chunk);
    }
    assert_eq!(transferred, ciphertext);
    assert_eq!(
        store.lock().unwrap().read_object(&address).unwrap(),
        ciphertext
    );
    assert!(
        review["actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|action| { action["kind"]["deployAgeSecret"]["ciphertext"] == address.0 })
    );
    assert!(!output.exists());
}

#[test]
fn interrupted_upload_resumes_and_bad_bytes_cannot_publish_a_review() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    let state = root.join("state");
    let shared = digest(b"shared");
    let second = digest(b"second");
    let prepare = json!({"preparePlan":{"request":{
        "repositoryId":"manager-a",
        "logicalHost":"server",
        "usedInputs":[],
        "resources":{"resources":[
            {"desired":{"file":{"path":root.join("a"),"state":{"present":{"digest":shared}}}}},
            {"desired":{"file":{"path":root.join("b"),"state":{"present":{"digest":second}}}}}
        ]}
    }}});

    let mut target = TargetProcess::start(&state);
    let response = target.request(prepare.clone());
    let missing = response["missingContents"]["digests"].as_array().unwrap();
    assert_eq!(missing.len(), 2);
    assert!(missing.contains(&json!(shared)) && missing.contains(&json!(second)));
    assert_eq!(
        target.request(upload(&shared, "c2hhcmVk", true)),
        json!("contentAccepted")
    );
    assert_eq!(
        target.request(upload(&second, "c2Vj", false)),
        json!("contentAccepted")
    );
    target.disconnect();

    let store = StateStore::open(&state).unwrap();
    assert_eq!(
        store
            .lock()
            .unwrap()
            .read_object(&dek_core::ContentDigest(shared))
            .unwrap(),
        b"shared"
    );
    assert_eq!(fs::read_dir(state.join("plans")).unwrap().count(), 0);

    let mut target = TargetProcess::start(&state);
    assert_eq!(
        target.request(prepare.clone()),
        json!({"missingContents":{"digests":[second]}})
    );
    let bad = target.request(upload(&second, "YmFk", true));
    assert!(
        bad["error"]["message"].as_str().unwrap().contains(&second),
        "{bad}"
    );
    assert_eq!(fs::read_dir(state.join("plans")).unwrap().count(), 0);
    target.disconnect();

    let mut target = TargetProcess::start(&state);
    assert_eq!(
        target.request(prepare),
        json!({"missingContents":{"digests":[second]}})
    );
    assert_eq!(
        target.request(upload(&second, "c2Vjb25k", true)),
        json!("contentAccepted")
    );
    let result = target.request(json!("finishPlan"));
    let plan: Plan = serde_json::from_value(result["planned"]["plan"].clone()).unwrap();
    let review = store
        .lock()
        .unwrap()
        .load_plan(
            &plan.id,
            PlanBinding {
                repository_id: "manager-a",
                logical_host: "server",
                target: &target.capabilities.target,
            },
        )
        .unwrap();
    assert_eq!(review.plan, plan);
    target.disconnect();
}

fn upload(digest: &str, chunk: &str, complete: bool) -> Value {
    json!({"uploadContent":{"digest":digest,"chunk":chunk,"complete":complete}})
}

struct TargetProcess {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    capabilities: TargetCapabilities,
    last_id: u64,
}

impl TargetProcess {
    fn start(state: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_dek"))
            .args(["__target-stdio", "--state-dir"])
            .arg(state)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        writeln!(input, "{}", json!({"hello":{"versions":[1]}})).unwrap();
        let hello = Self::read(&mut output);
        Self {
            child,
            input,
            output,
            capabilities: serde_json::from_value(hello["hello"]["capabilities"].clone()).unwrap(),
            last_id: 0,
        }
    }

    fn read(output: &mut BufReader<ChildStdout>) -> Value {
        let mut line = String::new();
        assert_ne!(
            output.read_line(&mut line).unwrap(),
            0,
            "target closed protocol output"
        );
        serde_json::from_str(&line).unwrap()
    }

    fn request(&mut self, operation: Value) -> Value {
        self.last_id += 1;
        writeln!(
            self.input,
            "{}",
            json!({"request":{"id":self.last_id,"operation":operation}})
        )
        .unwrap();
        let reply = Self::read(&mut self.output);
        assert_eq!(reply["response"]["id"], self.last_id);
        reply["response"]["result"].clone()
    }

    fn disconnect(self) {
        let Self {
            mut child, input, ..
        } = self;
        drop(input);
        assert!(child.wait().unwrap().success());
    }
}
