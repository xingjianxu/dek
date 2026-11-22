use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Command, Stdio},
    sync::mpsc,
    time::Duration,
};

use dek_core::{
    Action, ActionKind, ContentDigest, DesiredResource, FileContent, FileResource, FileState, Plan,
    PlanVersion, Resource, ResourceSet, ServiceEffect, TargetIdentity, UnitId, UnitScope,
};
use sha2::{Digest, Sha256};

use super::*;
use crate::{
    planner::{plan_id, resource_digest},
    state::StateStore,
};

fn guest() -> TargetIdentity {
    TargetIdentity {
        id: "test-machine".into(),
        os: "linux".into(),
        distribution: Some("arch".into()),
        architecture: "x86_64".into(),
    }
}

fn binding(target: &TargetIdentity) -> PlanBinding<'_> {
    PlanBinding {
        repository_id: "test-repo",
        logical_host: "server",
        target,
    }
}

fn save_review(guard: &StateGuard<'_>, target: &TargetIdentity) -> Plan {
    let reviewed = b"reviewed configuration";
    let content = FileContent {
        digest: ContentDigest(format!("sha256:{:x}", Sha256::digest(reviewed))),
        attributes: Default::default(),
    };
    let path = "/srv/config".to_owned();
    let resources = ResourceSet {
        resources: vec![Resource {
            desired: DesiredResource::File(FileResource {
                path: path.clone(),
                state: FileState::Present(content.clone()),
            }),
            origins: Vec::new(),
            depends_on: Vec::new(),
            notifications: Vec::new(),
        }],
    };
    let mut plan = Plan {
        format_version: PlanVersion::V1,
        id: String::new(),
        repository_id: binding(target).repository_id.into(),
        target: target.clone(),
        resource_digest: resource_digest(&resources).unwrap(),
        used_capabilities: Vec::new(),
        used_inputs: Vec::new(),
        actions: vec![
            Action {
                id: "write".into(),
                kind: ActionKind::WriteFile {
                    path,
                    content: content.clone(),
                },
                reason: "reconcile file".into(),
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
                    sources: Vec::new(),
                },
                reason: "reload after file update".into(),
                origins: Vec::new(),
                depends_on: vec!["write".into()],
                preconditions: Vec::new(),
            },
        ],
    };
    plan.id = plan_id(&plan, Some(binding(target).logical_host)).unwrap();
    guard
        .save_plan(
            binding(target).logical_host,
            &plan,
            &resources,
            &BTreeMap::from([(content.digest, reviewed.to_vec())]),
        )
        .unwrap();
    plan
}

#[test]
fn different_requests_get_distinct_runs_and_repeat_requests_get_original_id() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("state");
    let target = guest();
    let scope = binding(&target);
    let store = StateStore::open(&root).unwrap();
    let (first, second) = {
        let guard = store.lock().unwrap();
        let plan = save_review(&guard, &target);
        let first = guard.begin_run(&plan.id, scope, "first").unwrap();
        let second = guard.begin_run(&plan.id, scope, "second").unwrap();
        assert_ne!(first.id, second.id);
        assert_eq!(guard.begin_run(&plan.id, scope, "first").unwrap(), first);
        (first, second)
    };

    let query = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "state::runs::tests::query_runs_in_child"])
        .env("DEK_TEST_QUERY_RUN_STORE", &root)
        .env("DEK_TEST_QUERY_LATEST", &second.id)
        .env("DEK_TEST_QUERY_OLDER", &first.id)
        .output()
        .unwrap();
    assert!(
        query.status.success(),
        "{}{}",
        String::from_utf8_lossy(&query.stdout),
        String::from_utf8_lossy(&query.stderr)
    );

    let reopened = StateStore::open(&root).unwrap();
    let guard = reopened.lock().unwrap();
    assert_eq!(guard.latest_run(scope).unwrap(), Some(second));
    assert_eq!(guard.load_run(&first.id, scope).unwrap(), first);
    let mut other_machine = target.clone();
    other_machine.id = "different-machine".into();
    for other in [
        PlanBinding {
            target: &other_machine,
            ..scope
        },
        PlanBinding {
            repository_id: "other-repo",
            ..scope
        },
        PlanBinding {
            logical_host: "other-host",
            ..scope
        },
    ] {
        assert!(guard.load_run(&first.id, other).is_err());
        assert!(guard.latest_run(other).unwrap().is_none());
    }
}

#[test]
fn query_runs_in_child() {
    let Some(root) = std::env::var_os("DEK_TEST_QUERY_RUN_STORE") else {
        return;
    };
    let newest = std::env::var("DEK_TEST_QUERY_LATEST").unwrap();
    let earlier = std::env::var("DEK_TEST_QUERY_OLDER").unwrap();
    let store = StateStore::open(&PathBuf::from(root)).unwrap();
    let guard = store.lock().unwrap();
    let target = guest();
    let scope = binding(&target);
    let latest = guard.latest_run(scope).unwrap().unwrap();
    let older = guard.load_run(&earlier, scope).unwrap();
    assert_eq!(latest.id, newest);
    assert_eq!(older.id, earlier);
    assert_eq!(latest.plan_id, older.plan_id);
    assert_eq!(latest.actions, older.actions);
}

#[test]
fn record_attempt_in_child() {
    let Some(root) = std::env::var_os("DEK_TEST_RUN_STORE") else {
        return;
    };
    let run = std::env::var("DEK_TEST_RUN_ID").unwrap();
    let store = StateStore::open(&PathBuf::from(root)).unwrap();
    let guard = store.lock().unwrap();
    assert_eq!(
        guard
            .begin_action(&run, binding(&guest()), "write")
            .unwrap(),
        ActionAttempt::Started(1)
    );
    println!("attempt was durably recorded");
    std::io::stdout().flush().unwrap();
    loop {
        std::thread::park();
    }
}

#[test]
fn interrupted_attempt_stays_unverified_and_receipt_is_idempotent() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("state");
    let target = guest();
    let scope = binding(&target);
    let store = StateStore::open(&root).unwrap();
    let run = {
        let guard = store.lock().unwrap();
        let plan = save_review(&guard, &target);
        guard.begin_run(&plan.id, scope, "request").unwrap()
    };
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "state::runs::tests::record_attempt_in_child",
            "--nocapture",
        ])
        .env("DEK_TEST_RUN_STORE", &root)
        .env("DEK_TEST_RUN_ID", &run.id)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let output = BufReader::new(child.stdout.take().unwrap());
    let (sender, receiver) = mpsc::channel();
    let acknowledgement = std::thread::spawn(move || {
        let recorded = output
            .lines()
            .any(|line| line.unwrap().contains("attempt was durably recorded"));
        let _ = sender.send(recorded);
    });
    let recorded = receiver.recv_timeout(Duration::from_secs(10));
    let _ = child.kill();
    assert!(!child.wait().unwrap().success());
    acknowledgement.join().unwrap();
    assert_eq!(recorded, Ok(true));

    let reopened = StateStore::open(&root).unwrap();
    let guard = reopened.lock().unwrap();
    let observed = guard.load_run(&run.id, scope).unwrap();
    assert_eq!(observed.actions[0].attempt, 1);
    assert_eq!(observed.actions[0].result, ActionResult::Unverified);
    assert_eq!(
        guard.begin_action(&run.id, scope, "write").unwrap(),
        ActionAttempt::NeedsVerification(1)
    );
    for _ in 0..2 {
        guard
            .record_action_result(
                &run.id,
                scope,
                "write",
                1,
                ActionReceipt::Verified { changed: true },
            )
            .unwrap();
    }
    let observed = guard.load_run(&run.id, scope).unwrap();
    assert_eq!(observed.actions[0].attempt, 1);
    assert_eq!(
        observed.actions[0].result,
        ActionResult::Verified { changed: true }
    );
    assert_eq!(observed.actions[1].result, ActionResult::Pending);
}

#[test]
fn pending_reload_survives_failed_result_and_cache_deletion() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("state");
    let cache = temporary.path().join("controller-cache");
    fs::create_dir(&cache).unwrap();
    fs::write(cache.join("evaluation"), b"cached").unwrap();
    let target = guest();
    let scope = binding(&target);
    let store = StateStore::open(&root).unwrap();
    let run = {
        let guard = store.lock().unwrap();
        let plan = save_review(&guard, &target);
        let run = guard.begin_run(&plan.id, scope, "pending-reload").unwrap();
        guard.queue_notification(&run.id, scope, "reload").unwrap();
        let write = started(guard.begin_action(&run.id, scope, "write").unwrap());
        guard
            .record_action_result(
                &run.id,
                scope,
                "write",
                write,
                ActionReceipt::Verified { changed: true },
            )
            .unwrap();
        let reload = started(guard.begin_action(&run.id, scope, "reload").unwrap());
        guard
            .record_action_result(
                &run.id,
                scope,
                "reload",
                reload,
                ActionReceipt::Failed {
                    message: "test reload failed".into(),
                },
            )
            .unwrap();
        run
    };
    fs::remove_dir_all(cache).unwrap();

    let reopened = StateStore::open(&root).unwrap();
    let guard = reopened.lock().unwrap();
    let failed = guard.load_run(&run.id, scope).unwrap();
    assert_eq!(failed.plan_id, run.plan_id);
    assert_eq!(failed.pending_notifications[0].action_id, "reload");
    assert_eq!(failed.pending_notifications[0].depends_on, ["write"]);
    assert!(guard.finish_run(&run.id, scope).is_err());
    let reload = started(guard.begin_action(&run.id, scope, "reload").unwrap());
    guard
        .record_action_result(
            &run.id,
            scope,
            "reload",
            reload,
            ActionReceipt::Verified { changed: true },
        )
        .unwrap();
    guard.finish_run(&run.id, scope).unwrap();
    let recovered = guard.load_run(&run.id, scope).unwrap();
    assert!(recovered.finished);
    assert!(recovered.pending_notifications.is_empty());
}

fn started(attempt: ActionAttempt) -> u32 {
    let ActionAttempt::Started(number) = attempt else {
        panic!("unexpected unresolved attempt {attempt:?}");
    };
    number
}
