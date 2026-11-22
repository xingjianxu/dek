#![cfg(target_os = "linux")]

use std::{
    cell::Cell,
    collections::BTreeMap,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use dek_core::{
    Action, ActionKind, ContentDigest, DesiredResource, FileAttributes, FileContent,
    FileObservation, FileResource, FileState, Plan, PlanMode, Precondition,
    RepositoryPackageCandidate, RepositoryPackageChange, RepositoryTransaction,
    RepositoryTransactionScope, Resource, ResourceCapability, ResourceKind, ResourceSet,
    TargetIdentity,
};
use dek_target::{
    backend::{
        ActionContext, ActionReadiness, Backend, BackendError, BackendRegistry, PlannedAction,
        PlannedPackages, RepositoryPreparation,
    },
    executor::{ExecutionOutcome, ExecutionRequest, Executor},
    planner::plan_for_host_with_contents_mode,
    state::{ActionResult, PlanBinding, StateStore},
};
use sha2::{Digest, Sha256};

const FILE: &str = "/etc/dek-upgrade";
const REVIEWED: &[u8] = b"approved";
const GENERATED: &[u8] = b"created by package hook";

struct Target {
    installed: Rc<Cell<&'static str>>,
    candidate: Cell<&'static str>,
    content: Rc<Cell<&'static [u8]>>,
    package_writes: Rc<Cell<usize>>,
    config_writes: Cell<usize>,
    hook_changes_config: bool,
}

impl Target {
    fn new(hook_changes_config: bool) -> Self {
        Self {
            installed: Rc::new(Cell::new("1")),
            candidate: Cell::new("2"),
            content: Rc::new(Cell::new(REVIEWED)),
            package_writes: Rc::new(Cell::new(0)),
            config_writes: Cell::new(0),
            hook_changes_config,
        }
    }

    fn registry(&self) -> BackendRegistry<'_> {
        let mut registry = BackendRegistry::new(
            TargetIdentity {
                id: "upgrade-target".into(),
                os: "linux".into(),
                distribution: Some("arch".into()),
                architecture: "x86_64".into(),
            },
            1,
            |path| Ok(path.to_owned()),
        );
        registry.register(self).unwrap();
        registry
    }

    fn is_current(&self, action: &ActionKind) -> bool {
        match action {
            ActionKind::RepositoryTransaction(tx) => tx.changes.iter().all(|change| {
                matches!(change, RepositoryPackageChange::Upgrade { candidate, .. }
                    if candidate.version == self.installed.get())
            }),
            ActionKind::WriteFile { content, .. } => content.digest == digest(self.content.get()),
            other => panic!("unexpected action {other:?}"),
        }
    }
}

impl Backend for Target {
    fn capabilities(&self) -> Vec<ResourceCapability> {
        [ResourceKind::Package, ResourceKind::File]
            .into_iter()
            .map(|kind| ResourceCapability {
                kind,
                attributes: Default::default(),
                version: 1,
            })
            .collect()
    }

    fn validate(&self, _: &DesiredResource) -> Result<(), BackendError> {
        Ok(())
    }

    fn observe(&self, _: &DesiredResource) -> Result<Vec<Precondition>, BackendError> {
        Ok(vec![Precondition::File {
            path: FILE.into(),
            expected: FileObservation::Present(content(self.content.get())),
        }])
    }

    fn plan(
        &self,
        _: &DesiredResource,
        observed: &[Precondition],
    ) -> Result<Vec<PlannedAction>, BackendError> {
        let kind = ActionKind::WriteFile {
            path: FILE.into(),
            content: content(REVIEWED),
        };
        Ok(if self.is_current(&kind) {
            vec![]
        } else {
            vec![draft(kind, observed.to_vec())]
        })
    }

    fn plan_packages(
        &self,
        _: &ResourceSet,
        mode: PlanMode,
        _reviewed: Option<&[Action]>,
    ) -> Result<Option<PlannedPackages>, BackendError> {
        if mode == PlanMode::Normal {
            return Ok(None);
        }
        let changes = if self.installed.get() == self.candidate.get() {
            vec![]
        } else {
            vec![RepositoryPackageChange::Upgrade {
                installed_version: self.installed.get().into(),
                candidate: RepositoryPackageCandidate {
                    name: "fixture".into(),
                    version: self.candidate.get().into(),
                    repository: "fixture".into(),
                    sha256: "a".repeat(64),
                },
            }]
        };
        Ok(Some(PlannedPackages {
            transaction: Some(draft(
                ActionKind::RepositoryTransaction(RepositoryTransaction {
                    scope: RepositoryTransactionScope::Upgrade,
                    targets: vec![],
                    changes,
                }),
                vec![],
            )),
            ..PlannedPackages::default()
        }))
    }

    fn validate_action(&self, _: &Action) -> Result<(), BackendError> {
        Ok(())
    }

    fn inspect_action(&self, action: &Action) -> Result<ActionReadiness, BackendError> {
        Ok(if self.is_current(&action.kind) {
            ActionReadiness::Satisfied
        } else {
            ActionReadiness::Ready
        })
    }

    fn prepare_repository_transaction(
        &self,
        _: &Action,
    ) -> Result<RepositoryPreparation, BackendError> {
        let installed = Rc::clone(&self.installed);
        let content = Rc::clone(&self.content);
        let writes = Rc::clone(&self.package_writes);
        let candidate = self.candidate.get();
        let hook_changes_config = self.hook_changes_config;
        Ok(RepositoryPreparation::Ready(Box::new(move || {
            writes.set(writes.get() + 1);
            installed.set(candidate);
            if hook_changes_config {
                content.set(GENERATED);
            }
            Ok(())
        })))
    }

    fn execute(&self, action: &ActionContext<'_>) -> Result<(), BackendError> {
        match &action.action.kind {
            ActionKind::RepositoryTransaction(_) => panic!("expected guarded native commit"),
            ActionKind::WriteFile { content, .. } => {
                assert_eq!(action.content(&content.digest)?, REVIEWED);
                self.config_writes.set(self.config_writes.get() + 1);
                self.content.set(REVIEWED);
            }
            other => panic!("unexpected action {other:?}"),
        }
        Ok(())
    }

    fn verify(&self, action: &ActionKind) -> Result<bool, BackendError> {
        Ok(self.is_current(action))
    }
}

fn draft(kind: ActionKind, preconditions: Vec<Precondition>) -> PlannedAction {
    PlannedAction {
        kind,
        preconditions,
        reason: "fixture".into(),
        content_changed: false,
    }
}

fn digest(bytes: &[u8]) -> ContentDigest {
    ContentDigest(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn content(bytes: &[u8]) -> FileContent {
    FileContent {
        digest: digest(bytes),
        attributes: FileAttributes::default(),
    }
}

fn review(store: &StateStore, registry: &BackendRegistry<'_>) -> Plan {
    let resources = ResourceSet {
        resources: vec![Resource {
            desired: DesiredResource::File(FileResource {
                path: FILE.into(),
                state: FileState::Present(content(REVIEWED)),
            }),
            origins: vec![],
            depends_on: vec![],
            notifications: vec![],
        }],
    };
    let contents = BTreeMap::from([(digest(REVIEWED), REVIEWED.to_vec())]);
    let plan = plan_for_host_with_contents_mode(
        registry,
        "repo",
        "server",
        &resources,
        &[],
        &contents,
        PlanMode::Upgrade,
    )
    .unwrap();
    store
        .lock()
        .unwrap()
        .save_plan("server", &plan, &resources, &contents)
        .unwrap()
}

fn binding<'a>(registry: &'a BackendRegistry<'_>) -> PlanBinding<'a> {
    PlanBinding {
        repository_id: "repo",
        logical_host: "server",
        target: registry.target(),
    }
}

fn apply(
    executor: &Executor<'_, '_>,
    binding: PlanBinding<'_>,
    plan: &Plan,
    started: impl FnOnce(&str),
) -> ExecutionOutcome {
    executor
        .apply(
            ExecutionRequest {
                binding,
                plan_id: &plan.id,
                request_id: "upgrade-request",
            },
            |_| panic!("fixture uses no target inputs"),
            |id| {
                started(id);
                Ok(())
            },
        )
        .unwrap()
}

#[test]
fn changed_upgrade_candidate_requires_a_new_review_before_starting_a_run() {
    let fixture = tempfile::tempdir().unwrap();
    let target = Target::new(false);
    let registry = target.registry();
    let store = StateStore::open(&fixture.path().join("state")).unwrap();
    let old = review(&store, &registry);
    let binding = binding(&registry);
    target.candidate.set("3");

    let result = apply(&Executor::new(&store, &registry), binding, &old, |_| {
        panic!("no run should start")
    });
    let ExecutionOutcome::ReviewRequired {
        replacement: Some(new),
        ..
    } = result
    else {
        panic!("{result:?}");
    };
    assert_ne!(old.id, new.id);
    assert!(
        matches!(&new.repository_transaction().unwrap().changes[..], [
        RepositoryPackageChange::Upgrade { candidate, .. }
    ] if candidate.version == "3")
    );
    assert_eq!(target.installed.get(), "1");
    assert_eq!(target.package_writes.get(), 0);
    assert!(store.lock().unwrap().latest_run(binding).unwrap().is_none());
}

#[test]
fn package_created_configuration_is_returned_for_review_after_the_transaction_is_recorded() {
    let fixture = tempfile::tempdir().unwrap();
    let target = Target::new(true);
    let registry = target.registry();
    let store = StateStore::open(&fixture.path().join("state")).unwrap();
    let old = review(&store, &registry);
    let binding = binding(&registry);

    let result = apply(&Executor::new(&store, &registry), binding, &old, |_| {});
    let ExecutionOutcome::ReviewRequired {
        replacement: Some(new),
        ..
    } = result
    else {
        panic!("{result:?}");
    };
    assert_ne!(old.id, new.id);
    assert!(
        matches!(&new.actions[..], [action] if matches!(&action.kind, ActionKind::WriteFile { path, .. } if path == FILE))
    );
    let run = store.lock().unwrap().latest_run(binding).unwrap().unwrap();
    assert_eq!(run.plan_id, old.id);
    assert!(run.finished);
    assert_eq!(run.replacement_plan_id.as_deref(), Some(new.id.as_str()));
    assert_eq!(
        run.actions[0].result,
        ActionResult::Verified { changed: true }
    );
    assert_eq!(target.package_writes.get(), 1);
    assert_eq!(target.config_writes.get(), 0);
    assert_eq!(target.content.get(), GENERATED);

    let retry = apply(&Executor::new(&store, &registry), binding, &old, |id| {
        assert_eq!(id, run.id)
    });
    assert!(
        matches!(retry, ExecutionOutcome::ReviewRequired { replacement: Some(plan), .. } if plan.id == new.id)
    );
    assert_eq!(target.package_writes.get(), 1);
}

#[test]
fn unchanged_reviewed_configuration_runs_after_the_verified_upgrade() {
    let fixture = tempfile::tempdir().unwrap();
    let target = Target::new(false);
    target.content.set(GENERATED);
    let registry = target.registry();
    let store = StateStore::open(&fixture.path().join("state")).unwrap();
    let plan = review(&store, &registry);
    let binding = binding(&registry);
    let outcome = apply(&Executor::new(&store, &registry), binding, &plan, |_| {});
    assert!(
        matches!(outcome, ExecutionOutcome::Finished(run) if run.actions.len() == 2
        && run.actions.iter().all(|item| item.result == (ActionResult::Verified { changed: true })))
    );
    assert_eq!(target.package_writes.get(), 1);
    assert_eq!(target.config_writes.get(), 1);
    assert_eq!(target.content.get(), REVIEWED);
}

#[test]
fn expired_full_upgrade_retains_its_mode_after_plan_collection() {
    let fixture = tempfile::tempdir().unwrap();
    let target = Target::new(false);
    let registry = target.registry();
    let root = fixture.path().join("state");
    let clock = Arc::new(AtomicU64::new(1));
    let store = StateStore::open_with_clock(&root, {
        let clock = Arc::clone(&clock);
        move || clock.load(Ordering::SeqCst)
    })
    .unwrap();
    let old = review(&store, &registry);
    clock.store(8 * 24 * 60 * 60, Ordering::SeqCst);
    let new = review(&store, &registry);
    assert_ne!(old.id, new.id);
    assert!(!root.join("plans").join(format!("{}.mpk", old.id)).exists());
    let binding = binding(&registry);
    let result = apply(&Executor::new(&store, &registry), binding, &old, |_| {
        panic!("expired plan cannot start a run")
    });
    assert!(
        matches!(result, ExecutionOutcome::ReviewRequired { reason, replacement: None } if reason.replan_mode() == PlanMode::Upgrade)
    );
    assert_eq!(target.package_writes.get(), 0);
}
