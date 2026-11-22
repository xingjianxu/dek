use std::collections::{BTreeMap, BTreeSet};

use dek_core::{Action, ActionKind, TargetIdentity, UnitScope};

use super::{PlanError, action_id};

pub(super) fn coalesce_reload(
    mut actions: Vec<Action>,
    target: &TargetIdentity,
) -> Result<Vec<Action>, PlanError> {
    let mut definitions = Vec::new();
    let mut origins = Vec::new();
    for action in &actions {
        if let ActionKind::DeployUnit { unit, .. } = &action.kind
            && unit.scope == UnitScope::System
        {
            definitions.push(action.id.clone());
            for origin in &action.origins {
                if !origins.contains(origin) {
                    origins.push(origin.clone());
                }
            }
        }
    }
    if definitions.is_empty() {
        return Ok(actions);
    }

    let id = action_id(actions.len());
    for action in &mut actions {
        if matches!(
            &action.kind,
            ActionKind::SetUnitEnabled { unit, .. }
                | ActionKind::SetUnitRunning { unit, .. }
                | ActionKind::NotifyService { unit, .. }
                if unit.scope == UnitScope::System
        ) {
            action.depends_on.push(id.clone());
        }
    }
    actions.push(Action {
        id,
        kind: ActionKind::ReloadManager(UnitScope::System),
        reason: "本批系统级单元定义发生变化".into(),
        origins,
        depends_on: definitions,
        preconditions: Vec::new(),
    });
    order(actions, target)
}

fn order(actions: Vec<Action>, target: &TargetIdentity) -> Result<Vec<Action>, PlanError> {
    let count = actions.len();
    let indices: BTreeMap<_, _> = actions
        .iter()
        .enumerate()
        .map(|(index, action)| (action.id.as_str(), index))
        .collect();
    let mut successors = vec![Vec::new(); count];
    let mut pending = Vec::with_capacity(count);
    for (index, action) in actions.iter().enumerate() {
        pending.push(action.depends_on.len());
        for dependency in &action.depends_on {
            successors[indices[dependency.as_str()]].push(index);
        }
    }
    let mut ready: BTreeSet<_> = (0..count).filter(|&index| pending[index] == 0).collect();
    let mut actions: Vec<_> = actions.into_iter().map(Some).collect();
    let mut ordered = Vec::with_capacity(count);
    while let Some(index) = ready.pop_first() {
        ordered.push(
            actions[index]
                .take()
                .expect("ready action not yet consumed"),
        );
        for &next in &successors[index] {
            pending[next] -= 1;
            if pending[next] == 0 {
                ready.insert(next);
            }
        }
    }
    if ordered.len() != count {
        return Err(PlanError::Cycle {
            target: target.id.clone(),
            resources: "systemd manager reload and unit definitions".into(),
        });
    }
    Ok(ordered)
}

#[cfg(test)]
mod tests {
    use dek_core::{UnitDefinition, UnitId};

    use super::*;

    fn target() -> TargetIdentity {
        TargetIdentity {
            id: "unit-test".into(),
            os: "linux".into(),
            distribution: None,
            architecture: "x86_64".into(),
        }
    }

    fn unit(name: &str) -> UnitId {
        UnitId {
            scope: UnitScope::System,
            name: name.into(),
        }
    }

    fn action(index: usize, kind: ActionKind, dependencies: &[usize]) -> Action {
        Action {
            id: action_id(index),
            kind,
            reason: String::new(),
            origins: Vec::new(),
            depends_on: dependencies.iter().copied().map(action_id).collect(),
            preconditions: Vec::new(),
        }
    }

    #[test]
    fn manager_reloads_once_after_all_definitions_before_unit_effects() {
        let a = unit("a.service");
        let b = unit("b.timer");
        let records = vec![
            action(
                0,
                ActionKind::DeployUnit {
                    unit: a.clone(),
                    definition: UnitDefinition::default(),
                },
                &[],
            ),
            action(
                1,
                ActionKind::SetUnitRunning {
                    unit: a,
                    running: true,
                },
                &[0],
            ),
            action(
                2,
                ActionKind::DeployUnit {
                    unit: b.clone(),
                    definition: UnitDefinition::default(),
                },
                &[],
            ),
            action(
                3,
                ActionKind::SetUnitEnabled {
                    unit: b,
                    enabled: true,
                },
                &[2],
            ),
        ];
        let plan = coalesce_reload(records, &target()).unwrap();
        assert_eq!(
            plan.iter()
                .map(|action| action.id.as_str())
                .collect::<Vec<_>>(),
            ["action-1", "action-3", "action-5", "action-2", "action-4"]
        );
        assert_eq!(plan[2].kind, ActionKind::ReloadManager(UnitScope::System));
        assert_eq!(plan[2].depends_on, ["action-1", "action-3"]);
        assert!(
            plan[3..]
                .iter()
                .all(|action| action.depends_on.contains(&plan[2].id))
        );
    }

    #[test]
    fn reports_a_cycle_that_cannot_be_applied_with_one_manager_reload() {
        let a = unit("a.service");
        let b = unit("b.service");
        let records = vec![
            action(
                0,
                ActionKind::DeployUnit {
                    unit: a.clone(),
                    definition: UnitDefinition::default(),
                },
                &[],
            ),
            action(
                1,
                ActionKind::SetUnitRunning {
                    unit: a,
                    running: true,
                },
                &[0],
            ),
            action(
                2,
                ActionKind::DeployUnit {
                    unit: b,
                    definition: UnitDefinition::default(),
                },
                &[1],
            ),
        ];
        assert!(matches!(
            coalesce_reload(records, &target()),
            Err(PlanError::Cycle { .. })
        ));
    }
}
