use std::collections::{BTreeMap, BTreeSet};

use dek_core::{Action, ActionKind, Plan, Precondition, ResourceId, UnitScope};

use super::ExecutionError;
use crate::state::{ActionResult, RunRecord};

/// The reviewed program is ordered. Validate it before resolving live changes.
pub(super) fn validate_order(plan: &Plan) -> Result<(), ExecutionError> {
    let mut previous = BTreeSet::new();
    let mut repository_transaction = false;
    for action in &plan.actions {
        if matches!(action.kind, ActionKind::RepositoryTransaction(_))
            && std::mem::replace(&mut repository_transaction, true)
        {
            return Err(ExecutionError::action(action, "审阅计划只允许一项仓库事务"));
        }
        let bad_sources = if let ActionKind::NotifyService {
            effect, sources, ..
        } = &action.kind
        {
            let unique: BTreeSet<_> = sources
                .iter()
                .map(|source| (&source.action, source.when))
                .collect();
            unique.len() != sources.len()
                || sources
                    .iter()
                    .any(|source| !action.depends_on.contains(&source.action))
                || (!sources.is_empty()
                    && matches!(effect, dek_core::ServiceEffect::Restart)
                        != sources.iter().any(|source| {
                            matches!(source.effect, dek_core::ServiceEffect::Restart)
                        }))
        } else {
            false
        };
        if action.id.is_empty()
            || bad_sources
            || (matches!(action.kind, ActionKind::RepositoryTransaction(_)) && !previous.is_empty())
            || previous.contains(action.id.as_str())
            || action
                .depends_on
                .iter()
                .any(|dependency| !previous.contains(dependency.as_str()))
        {
            return Err(ExecutionError::action(
                action,
                "动作顺序或依赖无效，请重新规划",
            ));
        }
        previous.insert(action.id.as_str());
    }
    Ok(())
}

pub(super) enum Scope<'a> {
    Expanded,
    Reviewed {
        live: BTreeSet<&'a str>,
        conflict: Option<&'a Action>,
    },
}

/// Compare operations and their dependencies, whose plan-local ordinal IDs can
/// shift after an already-satisfied action disappears.
pub(super) fn match_scope<'a>(
    reviewed: &'a Plan,
    fresh: &Plan,
    run: Option<&RunRecord>,
) -> Scope<'a> {
    if reviewed.actions.iter().any(|notice| {
        matches!(&notice.kind, ActionKind::NotifyService { sources, .. } if sources.is_empty())
            && reviewed.actions.iter().any(|action| {
                matches!(
                    action.kind,
                    ActionKind::DeployAgeSecret(_) | ActionKind::SetAgeSecretAttributes(_)
                ) && notice.depends_on.contains(&action.id)
            })
    }) {
        // The planning-only age backend issued notices without source/effect
        // attribution. Their intended runtime effect cannot be recovered.
        return Scope::Expanded;
    }

    let approved_by_id: BTreeMap<_, _> = reviewed
        .actions
        .iter()
        .map(|action| (action.id.as_str(), action))
        .collect();
    let mut live = BTreeSet::new();
    let mut mapped = BTreeMap::<&str, &str>::new();
    let mut conflict = None;

    for planned in &fresh.actions {
        let Some(approved) = reviewed.actions.iter().find(|action| {
            equivalent_kind(&action.kind, &planned.kind) && !live.contains(action.id.as_str())
        }) else {
            return Scope::Expanded;
        };
        if !covered_notification_sources(approved, planned, &mapped)
            || planned.depends_on.iter().any(|id| {
                mapped.get(id.as_str()).is_none_or(|ancestor| {
                    !includes_dependency(approved, ancestor, &approved_by_id)
                })
            })
        {
            return Scope::Expanded;
        }
        if planned.preconditions != approved.preconditions
            && conflict.is_none()
            && !run.is_some_and(|run| {
                observed_after_user_creation(approved, planned, run, &approved_by_id)
            })
        {
            conflict = Some(approved);
        }
        live.insert(approved.id.as_str());
        mapped.insert(planned.id.as_str(), approved.id.as_str());
    }
    Scope::Reviewed { live, conflict }
}

fn equivalent_kind(approved: &ActionKind, planned: &ActionKind) -> bool {
    match (approved, planned) {
        (
            ActionKind::NotifyService {
                unit,
                effect,
                sources,
            },
            ActionKind::NotifyService {
                unit: other,
                effect: other_effect,
                ..
            },
        ) => unit == other && (!sources.is_empty() || effect == other_effect),
        _ => approved == planned,
    }
}

fn covered_notification_sources(
    approved: &Action,
    planned: &Action,
    mapped: &BTreeMap<&str, &str>,
) -> bool {
    let (
        ActionKind::NotifyService { sources, .. },
        ActionKind::NotifyService {
            sources: current, ..
        },
    ) = (&approved.kind, &planned.kind)
    else {
        return true;
    };
    current.iter().all(|source| {
        mapped.get(source.action.as_str()).is_some_and(|id| {
            if sources.is_empty() {
                approved.depends_on.iter().any(|action| action == id)
            } else {
                sources.iter().any(|original| {
                    original.action == *id
                        && original.effect == source.effect
                        && original.when == source.when
                })
            }
        })
    })
}

fn observed_after_user_creation(
    reviewed: &Action,
    fresh: &Action,
    run: &RunRecord,
    graph: &BTreeMap<&str, &Action>,
) -> bool {
    let scope = match &reviewed.kind {
        ActionKind::DeployUnit { unit, .. }
        | ActionKind::SetUnitEnabled { unit, .. }
        | ActionKind::SetUnitRunning { unit, .. } => &unit.scope,
        ActionKind::ReloadManager(scope) => scope,
        _ => return false,
    };
    let UnitScope::User { user } = scope else {
        return false;
    };
    let account = |condition: &Precondition| {
        matches!(condition,
        Precondition::ResourceState { resource: ResourceId::User(name), .. } if name == user)
    };
    !reviewed.preconditions.iter().any(account)
        && fresh.preconditions.iter().any(account)
        && fresh
            .preconditions
            .iter()
            .filter(|condition| !account(condition))
            .eq(&reviewed.preconditions)
        && run.actions.iter().any(|item| {
            matches!(&item.action.kind, ActionKind::EnsureUser(account) if account.name == *user)
                && matches!(item.result, ActionResult::Verified { .. })
                && includes_dependency(reviewed, &item.action.id, graph)
        })
}

fn includes_dependency(action: &Action, wanted: &str, actions: &BTreeMap<&str, &Action>) -> bool {
    let mut visited = BTreeSet::new();
    let mut pending: Vec<_> = action.depends_on.iter().map(String::as_str).collect();
    while let Some(id) = pending.pop() {
        if id == wanted {
            return true;
        }
        if visited.insert(id) {
            pending.extend(actions[id].depends_on.iter().map(String::as_str));
        }
    }
    false
}
