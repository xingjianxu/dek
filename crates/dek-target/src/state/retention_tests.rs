use std::{
    fs::{DirBuilder, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
};

use dek_core::{Action, ActionKind, FileContent, PlanVersion, ServiceEffect, UnitId, UnitScope};

use super::*;

struct Fixture {
    _root: tempfile::TempDir,
    store: StateStore,
    clock: Arc<AtomicU64>,
    target: TargetIdentity,
}

impl Fixture {
    fn new(ttl: Option<u64>) -> Self {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        DirBuilder::new().mode(0o700).create(&state).unwrap();
        if let Some(ttl) = ttl {
            let mut settings = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(state.join("retention.json"))
                .unwrap();
            write!(settings, r#"{{"planTtlSeconds":{ttl}}}"#).unwrap();
        }
        let clock = Arc::new(AtomicU64::new(100));
        let source = clock.clone();
        let store =
            StateStore::open_with_clock(&state, move || source.load(Ordering::SeqCst)).unwrap();
        Self {
            _root: root,
            store,
            clock,
            target: TargetIdentity {
                id: "target".into(),
                os: "linux".into(),
                distribution: None,
                architecture: "x86_64".into(),
            },
        }
    }

    fn time(&self, timestamp: u64) {
        self.clock.store(timestamp, Ordering::SeqCst);
    }

    fn binding<'a>(&'a self, host: &'a str) -> PlanBinding<'a> {
        PlanBinding {
            repository_id: "repo",
            logical_host: host,
            target: &self.target,
        }
    }

    fn save(&self, state: &StateGuard<'_>, host: &str, bytes: &[u8]) -> (Plan, ContentDigest) {
        let digest = ContentDigest(format!("sha256:{:x}", Sha256::digest(bytes)));
        let resources = ResourceSet::default();
        let mut plan = Plan {
            format_version: PlanVersion::V1,
            id: String::new(),
            repository_id: "repo".into(),
            target: self.target.clone(),
            resource_digest: resource_digest(&resources).unwrap(),
            used_capabilities: Vec::new(),
            used_inputs: Vec::new(),
            actions: vec![
                Action {
                    id: "write".into(),
                    kind: ActionKind::WriteFile {
                        path: "/tmp/config".into(),
                        content: FileContent {
                            digest: digest.clone(),
                            attributes: Default::default(),
                        },
                    },
                    reason: "reviewed".into(),
                    origins: Vec::new(),
                    depends_on: Vec::new(),
                    preconditions: Vec::new(),
                },
                Action {
                    id: "reload".into(),
                    kind: ActionKind::NotifyService {
                        unit: UnitId {
                            scope: UnitScope::System,
                            name: "example.service".into(),
                        },
                        effect: ServiceEffect::Reload,
                    },
                    reason: "trigger".into(),
                    origins: Vec::new(),
                    depends_on: vec!["write".into()],
                    preconditions: Vec::new(),
                },
            ],
        };
        plan.id = plan_id(&plan, Some(host)).unwrap();
        let saved = state
            .save_plan(
                host,
                &plan,
                &resources,
                &BTreeMap::from([(digest.clone(), bytes.to_vec())]),
            )
            .unwrap();
        (saved, digest)
    }
}

#[test]
fn expires_at_seven_days_by_default() {
    let f = Fixture::new(None);
    let state = f.store.lock().unwrap();
    let (plan, _) = f.save(&state, "a", b"content");
    f.time(100 + 604800 - 1);
    assert!(state.load_plan(&plan.id, f.binding("a")).is_ok());
    f.time(100 + 604800);
    assert!(matches!(
        state.load_plan(&plan.id, f.binding("a")),
        Err(StateError::ExpiredPlan { .. })
    ));
}

#[test]
fn renewal_keeps_live_shared_content_and_never_reuses_expired_identity() {
    let f = Fixture::new(Some(5));
    let state = f.store.lock().unwrap();
    let (a, shared) = f.save(&state, "a", b"shared");
    f.time(104);
    let (b, _) = f.save(&state, "b", b"shared");
    // Revisiting the unchanged active review reuses its ID without refreshing its age.
    assert_eq!(f.save(&state, "a", b"shared").0, a);
    f.time(105);
    let (renewed, _) = f.save(&state, "a", b"shared");
    assert_ne!(renewed.id, a.id);
    assert!(!f.store.plans.path.join(format!("{}.mpk", a.id)).exists());
    assert!(matches!(
        state.load_plan(&a.id, f.binding("a")),
        Err(StateError::ExpiredPlan { .. })
    ));
    assert!(state.load_plan(&a.id, f.binding("b")).is_err());
    assert_eq!(
        state.load_plan(&b.id, f.binding("b")).unwrap().contents[&shared],
        b"shared"
    );
    assert_eq!(f.save(&state, "a", b"shared").0, renewed);

    f.time(110);
    let (third, _) = f.save(&state, "a", b"shared");
    assert_ne!(third.id, a.id);
    assert_ne!(third.id, renewed.id);
    assert!(matches!(
        state.load_plan(&renewed.id, f.binding("a")),
        Err(StateError::ExpiredPlan { .. })
    ));
    assert_eq!(state.read_object(&shared).unwrap(), b"shared");
}

#[test]
fn unfinished_and_failed_runs_keep_expired_content_until_complete() {
    let f = Fixture::new(Some(5));
    let state = f.store.lock().unwrap();
    let scope = f.binding("a");
    let (a, digest) = f.save(&state, "a", b"only a");
    let run = state.begin_run(&a.id, scope, "execution-a").unwrap();
    f.time(105);
    f.save(&state, "b", b"b");
    assert!(matches!(
        state.load_plan(&a.id, scope),
        Err(StateError::ExpiredPlan { .. })
    ));
    assert!(
        state
            .begin_run(&a.id, scope, "a different execution")
            .is_err()
    );
    assert_eq!(
        state.load_run_plan(&run.id, scope).unwrap().contents[&digest],
        b"only a"
    );
    f.time(101);
    assert!(matches!(
        state.load_plan(&a.id, scope),
        Err(StateError::ExpiredPlan { .. })
    ));
    assert_eq!(
        state.load_run_plan(&run.id, scope).unwrap().contents[&digest],
        b"only a"
    );
    f.time(105);
    state.queue_notification(&run.id, scope, "reload").unwrap();
    assert_eq!(
        state.begin_action(&run.id, scope, "write").unwrap(),
        ActionAttempt::Started(1)
    );
    state
        .record_action_result(
            &run.id,
            scope,
            "write",
            1,
            ActionReceipt::Verified { changed: true },
        )
        .unwrap();
    assert_eq!(
        state.begin_action(&run.id, scope, "reload").unwrap(),
        ActionAttempt::Started(1)
    );
    state
        .record_action_result(
            &run.id,
            scope,
            "reload",
            1,
            ActionReceipt::Failed {
                message: "try again".into(),
            },
        )
        .unwrap();

    f.time(111);
    f.save(&state, "c", b"c");
    assert_eq!(
        state.load_run_plan(&run.id, scope).unwrap().contents[&digest],
        b"only a"
    );
    assert_eq!(
        state.begin_action(&run.id, scope, "reload").unwrap(),
        ActionAttempt::Started(2)
    );
    state
        .record_action_result(
            &run.id,
            scope,
            "reload",
            2,
            ActionReceipt::Verified { changed: true },
        )
        .unwrap();
    state.finish_run(&run.id, scope).unwrap();
    f.save(&state, "d", b"d");
    assert!(!state.has_object(&digest).unwrap());
    assert!(state.load_run(&run.id, scope).unwrap().finished);
}

#[test]
fn read_and_pin_serializes_with_collection() {
    let f = Fixture::new(Some(5));
    let state = f.store.lock().unwrap();
    let (plan, digest) = f.save(&state, "a", b"pinned");
    let scope = f.binding("a");
    f.time(104);
    let run = std::thread::scope(|threads| {
        let (started, receive) = mpsc::channel();
        let fixture = &f;
        let collected = threads.spawn(move || {
            started.send(()).unwrap();
            let other = fixture.store.lock().unwrap();
            fixture.save(&other, "b", b"new review");
        });
        receive.recv().unwrap();
        let reviewed = state.load_plan(&plan.id, scope).unwrap();
        assert_eq!(reviewed.contents[&digest], b"pinned");
        let run = state.begin_run(&plan.id, scope, "lock-race").unwrap();
        f.time(105);
        drop(state);
        collected.join().unwrap();
        run
    });
    let state = f.store.lock().unwrap();
    assert!(matches!(
        state.load_plan(&plan.id, scope),
        Err(StateError::ExpiredPlan { .. })
    ));
    assert_eq!(
        state.load_run_plan(&run.id, scope).unwrap().contents[&digest],
        b"pinned"
    );
}

#[test]
fn dangling_run_blocks_collection_before_deleting_other_plans() {
    let f = Fixture::new(Some(5));
    let state = f.store.lock().unwrap();
    let (pinned, _) = f.save(&state, "pinned", b"needed by run");
    state
        .begin_run(&pinned.id, f.binding("pinned"), "run")
        .unwrap();
    let (retired, content) = f.save(&state, "retired", b"expired content");
    let filename = state.plan_name(&pinned.id).unwrap();
    f.store.plans.remove(&filename).unwrap();

    f.time(105);
    let plan = Plan {
        format_version: PlanVersion::V1,
        id: String::new(),
        repository_id: "repo".into(),
        target: f.target.clone(),
        resource_digest: resource_digest(&ResourceSet::default()).unwrap(),
        used_capabilities: Vec::new(),
        used_inputs: Vec::new(),
        actions: Vec::new(),
    };
    let plan = Plan {
        id: plan_id(&plan, Some("new")).unwrap(),
        ..plan
    };
    let err = state
        .save_plan("new", &plan, &ResourceSet::default(), &BTreeMap::new())
        .unwrap_err();
    assert!(err.to_string().contains(&pinned.id), "{err}");
    assert!(
        f.store
            .plans
            .path
            .join(format!("{}.mpk", retired.id))
            .is_file()
    );
    assert!(state.has_object(&content).unwrap());
}

#[test]
fn persisted_retirement_prevents_clock_rollback_from_resurrecting_a_plan() {
    let f = Fixture::new(Some(5));
    let state = f.store.lock().unwrap();
    let (old, _) = f.save(&state, "old", b"shared");
    let filename = state.plan_name(&old.id).unwrap();
    let interrupted_record = f.store.plans.read(&filename).unwrap().unwrap();

    f.time(105);
    f.save(&state, "other", b"shared");
    // Restore the source record to represent an interruption after the marker was
    // durably published and before the expired plan file was unlinked.
    assert!(f.store.expired.read(&filename).unwrap().is_some());
    f.store
        .plans
        .publish(&filename, &interrupted_record)
        .unwrap();
    f.time(101);
    assert!(matches!(
        state.load_plan(&old.id, f.binding("old")),
        Err(StateError::ExpiredPlan { .. })
    ));
    let (new, _) = f.save(&state, "old", b"shared");
    assert_ne!(new.id, old.id);
    assert!(f.store.plans.read(&filename).unwrap().is_none());
}
