use std::{
    env,
    fs::{self, File, TryLockError},
    io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Barrier, mpsc},
    thread,
    time::{Duration, Instant},
};

use dek_controller::cache::{
    CacheDependency, CacheDigest, CacheEntry, CacheStore, Publication, UnitKey, UnitStage,
    canonical_set, semantic_digest,
};
use dek_core::{InputKind, InputObservation, Observation, UnitSpec};
use serde_json::{Value, json};
use tempfile::{TempDir, tempdir};

const SIGNATURE: &str = "json-v1";
const CHILD_ROOT: &str = "DEK_TEST_INTERRUPTED_CACHE_ROOT";

fn test_store() -> (TempDir, CacheStore) {
    let root = tempdir().unwrap();
    let store = CacheStore::new(&root.path().join("cache"), root.path()).unwrap();
    (root, store)
}

fn key(unit: &str) -> UnitKey {
    UnitKey {
        stage: UnitStage::Collect,
        unit: unit.into(),
        params: Default::default(),
        target: None,
    }
}

fn entry(output: Value) -> CacheEntry<Value> {
    CacheEntry {
        dependencies: vec![],
        semantic_digest: semantic_digest(&output),
        output,
    }
}

fn publish(store: &CacheStore, key: &UnitKey, output: Value) {
    let lookup = store.lookup::<Value>(key, SIGNATURE).unwrap();
    assert_eq!(
        store
            .publish(&lookup, &entry(output), |_| Ok(true))
            .unwrap(),
        Publication::Published
    );
}

fn cached_output(store: &CacheStore, key: &UnitKey) -> Option<Value> {
    store
        .lookup::<Value>(key, SIGNATURE)
        .unwrap()
        .entry
        .map(|entry| entry.output)
}

#[test]
fn worktree_and_unit_paths_are_stable_and_isolated() {
    let home = tempdir().unwrap();
    let cache = home.path().join("cache");
    let first = home.path().join("first");
    let second = home.path().join("second");
    for root in [&first, &second] {
        fs::create_dir(root).unwrap();
        fs::write(root.join("base.json"), b"1").unwrap();
    }

    let first_store = CacheStore::new(&cache, &first).unwrap();
    let second_store = CacheStore::new(&cache, &second).unwrap();
    let key = key("base.json");
    let path = first_store.record_path(&key);
    let second_path = second_store.record_path(&key);
    let relative: Vec<_> = path.strip_prefix(&cache).unwrap().iter().collect();
    assert_eq!(relative[0], "dek");
    assert_eq!(relative[2], "units");
    assert_eq!(relative[1].len(), 64);
    let filename = relative[4].to_str().unwrap();
    assert_eq!(filename.len(), 68);
    assert_eq!(relative[3], &filename[..2]);
    assert_eq!(path.file_name(), second_path.file_name());
    assert_ne!(path, second_path);
    assert_eq!(
        CacheStore::new(&cache, &first.join("."))
            .unwrap()
            .record_path(&key),
        path
    );

    publish(&first_store, &key, json!(1));
    assert_eq!(cached_output(&second_store, &key), None);
    fs::write(first.join("base.json"), b"2").unwrap();
    publish(&first_store, &key, json!(2));
    assert_eq!(first_store.record_path(&key), path);
    assert_eq!(cached_output(&first_store, &key), Some(json!(2)));
}

#[test]
fn unit_key_includes_stage_nested_parameters_and_target_scope() {
    let (_root, store) = test_store();
    let mut first = key("profile:backup");
    first.params.insert(
        "config".into(),
        serde_json::from_str(r#"{"a":1,"b":2}"#).unwrap(),
    );
    let mut second = key("profile:backup");
    second.params.insert(
        "config".into(),
        serde_json::from_str(r#"{"b":2,"a":1}"#).unwrap(),
    );
    assert_eq!(store.record_path(&first), store.record_path(&second));

    second.params.insert("config".into(), json!({"a":1,"b":3}));
    assert_ne!(store.record_path(&first), store.record_path(&second));
    second = first.clone();
    second.target = Some("another-target".into());
    assert_ne!(store.record_path(&first), store.record_path(&second));
    second = first.clone();
    second.stage = UnitStage::Merge;
    assert_ne!(store.record_path(&first), store.record_path(&second));
}

#[test]
fn semantic_digest_normalizes_objects_and_explicit_sets_only() {
    let first = serde_json::from_str(r#"{"b":[1,2],"a":{"y":2,"x":1}}"#).unwrap();
    let second = serde_json::from_str(r#"{"a":{"x":1,"y":2},"b":[1,2]}"#).unwrap();
    let reversed = serde_json::from_str(r#"{"a":{"x":1,"y":2},"b":[2,1]}"#).unwrap();
    assert_eq!(semantic_digest(&first), semantic_digest(&second));
    assert_ne!(semantic_digest(&first), semantic_digest(&reversed));

    let array = |value| vec![json!({"members":value}), json!("host")];
    let first = json!({ "members": canonical_set(array(vec![1, 2])) });
    let second = json!({ "members": canonical_set(array(vec![1, 2]).into_iter().rev()) });
    let duplicate = json!({ "members": canonical_set([json!("host"), json!({"members":[1,2]}), json!("host")]) });
    let ordered_child = json!({ "members": canonical_set(array(vec![2, 1])) });
    assert_eq!(semantic_digest(&first), semantic_digest(&second));
    assert_eq!(semantic_digest(&first), semantic_digest(&duplicate));
    assert_ne!(semantic_digest(&first), semantic_digest(&ordered_child));
}

#[test]
fn compact_record_preserves_typed_output_and_dependency_identity() {
    let (_root, store) = test_store();
    let unit = key("entry");
    let spec: UnitSpec =
        serde_json::from_str(include_str!("../../dek-core/tests/fixtures/unit-spec.json")).unwrap();
    let record = CacheEntry {
        dependencies: vec![
            CacheDependency::Input(spec.inputs[1].clone()),
            CacheDependency::Unit {
                key: key("upstream"),
                semantic_digest: semantic_digest(&json!("upstream-output")),
            },
            CacheDependency::Options {
                paths: Some(vec!["programs.git.config".parse().unwrap()]),
                semantic_digest: semantic_digest(&json!({"init":{"defaultBranch":"main"}})),
            },
        ],
        semantic_digest: semantic_digest(&json!("normalized-fixture")),
        output: spec,
    };
    let lookup = store.lookup::<UnitSpec>(&unit, SIGNATURE).unwrap();
    assert_eq!(
        store
            .publish(&lookup, &record, |dependencies| {
                assert_eq!(dependencies, record.dependencies);
                Ok(true)
            })
            .unwrap(),
        Publication::Published
    );

    let bytes = fs::read(store.record_path(&unit)).unwrap();
    let (version, encoded_key, signature, encoded_record): (
        u32,
        UnitKey,
        String,
        CacheEntry<UnitSpec>,
    ) = rmp_serde::from_slice(&bytes).unwrap();
    assert_eq!(
        (version, encoded_key, signature),
        (1, unit.clone(), SIGNATURE.into())
    );
    assert_eq!(encoded_record, record);
    assert!(
        bytes
            .windows(b"defaultBranch".len())
            .any(|bytes| bytes == b"defaultBranch")
    );
    assert!(
        !bytes
            .windows(b"evaluator_signature".len())
            .any(|bytes| bytes == b"evaluator_signature")
    );
    assert_eq!(
        store.lookup::<UnitSpec>(&unit, SIGNATURE).unwrap().entry,
        Some(record)
    );
}

#[test]
fn signature_version_and_corrupt_records_are_local_misses() {
    let (_root, store) = test_store();
    let changing = key("changing");
    let intact = key("intact");
    publish(&store, &changing, json!("first"));
    publish(&store, &intact, json!("independent"));
    assert!(
        store
            .lookup::<Value>(&changing, "json-v2")
            .unwrap()
            .entry
            .is_none()
    );
    assert_eq!(cached_output(&store, &changing), Some(json!("first")));

    let path = store.record_path(&changing);
    let good = fs::read(&path).unwrap();
    let wrong_version =
        rmp_serde::to_vec(&(99_u32, &changing, SIGNATURE, entry(json!(true)))).unwrap();
    let wrong_key = rmp_serde::to_vec(&(1_u32, &intact, SIGNATURE, entry(json!(true)))).unwrap();
    let mut trailing = good.clone();
    trailing.push(0);
    for corrupt in [
        wrong_version,
        wrong_key,
        good[..good.len() / 2].to_vec(),
        trailing,
    ] {
        fs::write(&path, corrupt).unwrap();
        assert_eq!(cached_output(&store, &changing), None);
        assert_eq!(cached_output(&store, &intact), Some(json!("independent")));
        publish(&store, &changing, json!("rebuilt"));
        assert_eq!(cached_output(&store, &changing), Some(json!("rebuilt")));
    }
}

#[test]
fn older_calculation_cannot_replace_newer_publication() {
    let (_root, store) = test_store();
    let unit = key("race");
    publish(&store, &unit, json!("initial"));
    let both_read = Barrier::new(2);
    let (finished, wait_for_newer) = mpsc::channel();

    let older = store.lookup::<Value>(&unit, SIGNATURE).unwrap();
    let newer = store.lookup::<Value>(&unit, SIGNATURE).unwrap();
    thread::scope(|scope| {
        let old_store = &store;
        let old_barrier = &both_read;
        let old = scope.spawn(move || {
            old_barrier.wait();
            wait_for_newer.recv_timeout(Duration::from_secs(5)).unwrap();
            old_store
                .publish(&older, &entry(json!("old")), |_| Ok(true))
                .unwrap()
        });

        both_read.wait();
        let outcome = store.publish(&newer, &entry(json!("new")), |_| Ok(true));
        finished.send(()).unwrap();
        assert_eq!(outcome.unwrap(), Publication::Published);
        assert_eq!(old.join().unwrap(), Publication::Superseded);
    });
    assert_eq!(cached_output(&store, &unit), Some(json!("new")));
}

#[test]
fn publication_rechecks_observed_inputs_even_without_another_writer() {
    let (root, store) = test_store();
    let unit = key("local-input");
    fs::write(root.path().join("input"), b"before").unwrap();
    publish(&store, &unit, json!("initial"));
    let lookup = store.lookup::<Value>(&unit, SIGNATURE).unwrap();
    let mut record = entry(json!("candidate"));
    record.dependencies = vec![CacheDependency::Input(InputObservation {
        kind: InputKind::File,
        id: "input".into(),
        observation: Observation::Digest(CacheDigest::of_bytes(b"before").to_string()),
        origin: None,
    })];
    fs::write(root.path().join("input"), b"after").unwrap();
    let outcome =
        store
            .publish(&lookup, &record, |dependencies| {
                let [CacheDependency::Input(input)] = dependencies else {
                    panic!("expected input observation");
                };
                let actual = fs::read(root.path().join(&input.id))?;
                Ok(input.observation
                    == Observation::Digest(CacheDigest::of_bytes(&actual).to_string()))
            })
            .unwrap();
    assert_eq!(outcome, Publication::InputsChanged);
    assert_eq!(cached_output(&store, &unit), Some(json!("initial")));

    let error = store.publish(&lookup, &record, |_| {
        Err(io::Error::from(io::ErrorKind::Interrupted).into())
    });
    assert!(error.is_err());
    assert_eq!(cached_output(&store, &unit), Some(json!("initial")));
}

#[test]
fn killed_writer_releases_the_lock_and_preserves_the_previous_record() {
    let (root, store) = test_store();
    let unit = key("interrupted");
    publish(&store, &unit, json!("complete"));

    let mut child = Command::new(env::current_exe().unwrap())
        .args(["--exact", "interrupted_writer_child"])
        .env(CHILD_ROOT, root.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let marker = root.path().join("child-staged");
    while !marker.is_file() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    let ready = marker.is_file();
    let during_write = store.lookup::<Value>(&unit, SIGNATURE);
    let lock = File::options()
        .write(true)
        .open(store.record_path(&unit).with_extension("lock"));
    let lock_result = lock.as_ref().map(File::try_lock);
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());
    assert!(ready, "child did not reach input recheck");
    assert!(matches!(lock_result, Ok(Err(TryLockError::WouldBlock))));
    drop(lock);
    assert_eq!(
        during_write.unwrap().entry.unwrap().output,
        json!("complete")
    );
    assert_eq!(cached_output(&store, &unit), Some(json!("complete")));
    publish(&store, &unit, json!("recovered"));
    assert_eq!(cached_output(&store, &unit), Some(json!("recovered")));
}

#[test]
fn interrupted_writer_child() {
    let Some(root) = env::var_os(CHILD_ROOT) else {
        return;
    };
    let root = PathBuf::from(root);
    interrupted_writer(&root);
}

fn interrupted_writer(root: &Path) {
    let store = CacheStore::new(&root.join("cache"), root).unwrap();
    let unit = key("interrupted");
    let lookup = store.lookup::<Value>(&unit, SIGNATURE).unwrap();
    let result = store.publish(&lookup, &entry(json!("incomplete")), |_| {
        fs::write(root.join("child-staged"), b"1")?;
        thread::sleep(Duration::from_secs(20));
        Err(io::Error::from(io::ErrorKind::Interrupted).into())
    });
    assert!(result.is_err());
}
