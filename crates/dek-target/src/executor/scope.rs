use std::collections::{BTreeMap, BTreeSet};

use dek_core::{Action, ActionKind, Plan, Precondition, ResourceId, UnitScope};

use super::ExecutionError;
use crate::state::{ActionResult, RunRecord};

/// The reviewed program is ordered. Validate it before resolving live changes.
pub(super) fn validate_order(plan: &Plan) -> Result<(), ExecutionError> {
    let mut previous = BTreeSet::new();
    for action in &plan.actions {
        if action.id.is_empty()
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
    let approved_by_id: BTreeMap<_, _> = reviewed
        .actions
        .iter()
        .map(|action| (action.id.as_str(), action))
        .collect();
    let mut live = BTreeSet::new();
    let mut mapped = BTreeMap::<&str, &str>::new();
    let mut conflict = None;

    for planned in &fresh.actions {
        let Some(approved) = reviewed
            .actions
            .iter()
            .find(|action| action.kind == planned.kind && !live.contains(action.id.as_str()))
        else {
            return Scope::Expanded;
        };
        if planned.depends_on.iter().any(|id| {
            mapped
                .get(id.as_str())
                .is_none_or(|ancestor| !includes_dependency(approved, ancestor, &approved_by_id))
        }) {
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
