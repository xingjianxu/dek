use std::collections::{BTreeMap, HashSet};

use dek_core::{
    EffectiveOption, HostConfig, OptionDefinition, OptionPath, Origin, Priority, UnitSpec,
};
use serde_json::Value;
use thiserror::Error;

use super::SchemaRegistry;
use super::schema::{MergeStrategy, RegisteredOption, ValueMismatch, ValueType};

#[derive(Debug, Error)]
pub enum MergeError {
    #[error("unknown option {path} at {origin}")]
    UnknownOption { path: OptionPath, origin: Origin },
    #[error("option {path} at {origin} {mismatch}")]
    InvalidValue {
        path: OptionPath,
        origin: Box<Origin>,
        mismatch: ValueMismatch,
    },
    #[error(
        "conflicting definitions of option {path} at priority {priority:?}: {first} and {second}"
    )]
    Conflict {
        path: OptionPath,
        priority: Priority,
        first: Box<Origin>,
        second: Box<Origin>,
    },
}

impl SchemaRegistry {
    /// Merge the supplied profile units in graph order, followed by the host.
    /// A named option exists once a concrete path has been assigned.
    pub fn merge(&self, profiles: &[UnitSpec], host: &UnitSpec) -> Result<HostConfig, MergeError> {
        let mut groups = BTreeMap::new();
        for registered in &self.options {
            if !registered.dynamic
                && let Some(default) = registered.default_definition()
            {
                groups.insert(registered.schema.path.clone(), (registered, vec![default]));
            }
        }

        let units = profiles
            .iter()
            .map(|unit| (Priority::Profile, unit))
            .chain(std::iter::once((Priority::Host, host)));
        for (priority, unit) in units {
            for assignment in &unit.options {
                let registered =
                    self.lookup(&assignment.path)
                        .ok_or_else(|| MergeError::UnknownOption {
                            path: assignment.path.clone(),
                            origin: assignment.origin.clone(),
                        })?;
                if let Some(mismatch) = registered.schema.value_type.mismatch(&assignment.value) {
                    return Err(MergeError::InvalidValue {
                        path: assignment.path.clone(),
                        origin: Box::new(assignment.origin.clone()),
                        mismatch,
                    });
                }
                let (_, definitions) = groups.entry(assignment.path.clone()).or_insert_with(|| {
                    (
                        registered,
                        registered.default_definition().into_iter().collect(),
                    )
                });
                definitions.push(OptionDefinition {
                    value: assignment.value.clone(),
                    origin: assignment.origin.clone(),
                    priority,
                });
            }
        }

        let options = groups
            .into_iter()
            .map(|(path, (option, definitions))| {
                merge_option(&path, option, definitions).map(|effective| (path, effective))
            })
            .collect::<Result<_, _>>()?;
        Ok(HostConfig { options })
    }
}

fn merge_option(
    path: &OptionPath,
    option: &RegisteredOption,
    definitions: Vec<OptionDefinition>,
) -> Result<EffectiveOption, MergeError> {
    match option.schema.merge {
        MergeStrategy::Replace => merge_replacement(path, &option.schema.value_type, definitions),
        MergeStrategy::Set | MergeStrategy::Ordered => {
            let ValueType::List { list } = &option.schema.value_type else {
                unreachable!("list merge schema validated during registration");
            };
            let value = if matches!(option.schema.merge, MergeStrategy::Set) {
                let mut seen = HashSet::new();
                list_items(&definitions)
                    .filter(|(value, origin)| seen.insert(list.comparison_value(value, origin)))
                    .map(|(value, _)| value.clone())
                    .collect()
            } else {
                list_items(&definitions)
                    .map(|(value, _)| value.clone())
                    .collect()
            };
            Ok(EffectiveOption {
                value: Value::Array(value),
                priority: highest_priority(&definitions),
                effective_definitions: definitions,
                overridden_definitions: Vec::new(),
            })
        }
    }
}

fn merge_replacement(
    path: &OptionPath,
    value_type: &ValueType,
    mut definitions: Vec<OptionDefinition>,
) -> Result<EffectiveOption, MergeError> {
    for layer in definitions.chunk_by(|left, right| left.priority == right.priority) {
        let (first, others) = layer.split_first().expect("nonempty priority layer");
        let first_value = value_type.comparison_value(&first.value, &first.origin);
        if let Some(second) = others
            .iter()
            .find(|other| value_type.comparison_value(&other.value, &other.origin) != first_value)
        {
            return Err(MergeError::Conflict {
                path: path.clone(),
                priority: first.priority,
                first: Box::new(first.origin.clone()),
                second: Box::new(second.origin.clone()),
            });
        }
    }
    let priority = highest_priority(&definitions);
    let boundary = definitions.partition_point(|definition| definition.priority < priority);
    let effective_definitions = definitions.split_off(boundary);
    Ok(EffectiveOption {
        value: effective_definitions[0].value.clone(),
        priority,
        effective_definitions,
        overridden_definitions: definitions,
    })
}

fn highest_priority(definitions: &[OptionDefinition]) -> Priority {
    definitions
        .last()
        .expect("option has at least one definition")
        .priority
}

fn list_items(definitions: &[OptionDefinition]) -> impl Iterator<Item = (&Value, &Origin)> {
    definitions.iter().flat_map(|definition| {
        definition
            .value
            .as_array()
            .expect("validated list assignment")
            .iter()
            .map(|value| (value, &definition.origin))
    })
}
