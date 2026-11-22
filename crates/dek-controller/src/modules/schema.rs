use std::{
    borrow::Cow,
    fmt,
    path::{Path, PathBuf},
};

use dek_core::{OptionDefinition, OptionPath, Origin, Priority};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};

use crate::cache::semantic_digest;
use thiserror::Error;

#[derive(Debug, Default)]
pub struct SchemaRegistry {
    pub(super) options: Vec<RegisteredOption>,
}

/// Where a module registered an option definition.
#[derive(Clone, Debug)]
pub struct SchemaLocation {
    pub module: String,
    pub origin: Origin,
}

impl fmt::Display for SchemaLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "module {:?} at {}", self.module, self.origin)
    }
}

#[derive(Debug, Error)]
pub enum SchemaError {
    #[error("invalid schema for module {module:?} at {file}: {source}")]
    InvalidJson {
        module: String,
        file: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("invalid schema for option {path} from {location}: {message}")]
    InvalidOption {
        path: OptionPath,
        location: Box<SchemaLocation>,
        message: String,
    },
    #[error("option {path} from {second} overlaps {existing_path} from {first}")]
    DuplicateOption {
        path: OptionPath,
        existing_path: OptionPath,
        first: Box<SchemaLocation>,
        second: Box<SchemaLocation>,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SchemaFile {
    options: Vec<OptionSchema>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OptionSchema {
    pub(super) path: OptionPath,
    #[serde(rename = "type")]
    pub(super) value_type: ValueType,
    #[serde(default, deserialize_with = "present_value")]
    default: Option<Value>,
    #[serde(default)]
    pub(super) merge: MergeStrategy,
    description: String,
}

fn present_value<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum MergeStrategy {
    #[default]
    Replace,
    Set,
    Ordered,
    Map,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged, deny_unknown_fields)]
pub(super) enum ValueType {
    Primitive(PrimitiveType),
    List { list: Box<Self> },
    Map { map: Box<Self> },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum PrimitiveType {
    Any,
    Boolean,
    String,
    Integer,
    Number,
    SourceFile,
}

#[derive(Debug)]
pub(super) struct RegisteredOption {
    pub(super) schema: OptionSchema,
    location: SchemaLocation,
    default: Option<OptionDefinition>,
    pub(super) dynamic: bool,
}

impl SchemaRegistry {
    pub(super) fn merge_signature(&self) -> String {
        let schemas = self
            .options
            .iter()
            .map(|option| {
                json!([
                    option.schema.path,
                    option.schema.value_type,
                    option.schema.merge,
                    option.default
                ])
            })
            .collect::<Vec<_>>();
        format!("rust-merge-v1:{}", semantic_digest(&Value::Array(schemas)))
    }

    /// Register one static schema document atomically. `file` is the source path
    /// used for diagnostics and for relative defaults of type `sourceFile`.
    pub fn register_json(
        &mut self,
        module: &str,
        file: &str,
        json: &str,
    ) -> Result<(), SchemaError> {
        let schema: SchemaFile =
            serde_json::from_str(json).map_err(|source| SchemaError::InvalidJson {
                module: module.into(),
                file: file.into(),
                source,
            })?;

        let mut pending: Vec<RegisteredOption> = Vec::new();
        for (index, schema) in schema.options.into_iter().enumerate() {
            let location = SchemaLocation {
                module: module.into(),
                origin: json_origin(file, format!("/options/{index}")),
            };
            let registered = RegisteredOption::new(schema, location, || {
                json_origin(file, format!("/options/{index}/default"))
            })?;
            if let Some(existing) = self.options.iter().chain(&pending).find(|existing| {
                paths_match(
                    &existing.schema.path,
                    &registered.schema.path,
                    |left, right| wildcard(left) || wildcard(right),
                )
            }) {
                return Err(SchemaError::DuplicateOption {
                    path: registered.schema.path,
                    existing_path: existing.schema.path.clone(),
                    first: Box::new(existing.location.clone()),
                    second: Box::new(registered.location),
                });
            }
            pending.push(registered);
        }
        self.options.extend(pending);
        Ok(())
    }

    pub(super) fn lookup(&self, path: &OptionPath) -> Option<&RegisteredOption> {
        self.options.iter().find(|candidate| {
            if candidate.dynamic {
                paths_match(&candidate.schema.path, path, |pattern, _| wildcard(pattern))
            } else {
                &candidate.schema.path == path
            }
        })
    }
}

impl RegisteredOption {
    fn new(
        mut schema: OptionSchema,
        location: SchemaLocation,
        default_origin: impl FnOnce() -> Origin,
    ) -> Result<Self, SchemaError> {
        let invalid = |message| SchemaError::InvalidOption {
            path: schema.path.clone(),
            location: Box::new(location.clone()),
            message,
        };
        let mut dynamic = false;
        for component in schema.path.as_str().split('.') {
            if component.contains(['<', '>']) {
                if !wildcard(component) {
                    return Err(invalid(format!("invalid named component {component:?}")));
                }
                dynamic = true;
            }
        }
        if schema.description.trim().is_empty() {
            return Err(invalid("description must be nonempty".into()));
        }
        if matches!(schema.merge, MergeStrategy::Set | MergeStrategy::Ordered)
            && !matches!(schema.value_type, ValueType::List { .. })
        {
            return Err(invalid(
                "set and ordered merge strategies require a list type".into(),
            ));
        }
        if matches!(schema.merge, MergeStrategy::Map)
            && !matches!(schema.value_type, ValueType::Map { .. })
        {
            return Err(invalid("map merge strategy requires a map type".into()));
        }
        let default = if let Some(value) = schema.default.take() {
            if let Some(mismatch) = schema.value_type.mismatch(&value) {
                return Err(invalid(format!("default {mismatch}")));
            }
            Some(OptionDefinition {
                value,
                origin: default_origin(),
                priority: Priority::ModuleDefault,
            })
        } else {
            None
        };
        Ok(Self {
            schema,
            location,
            default,
            dynamic,
        })
    }

    pub(super) fn default_definition(&self) -> Option<OptionDefinition> {
        self.default.clone()
    }
}

fn json_origin(file: &str, pointer: String) -> Origin {
    Origin::Json {
        file: file.into(),
        pointer: pointer.try_into().expect("generated schema JSON Pointer"),
    }
}

fn wildcard(component: &str) -> bool {
    component
        .strip_prefix('<')
        .and_then(|name| name.strip_suffix('>'))
        .is_some_and(|name| {
            !name.is_empty()
                && name.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
                })
        })
}

fn paths_match(
    left: &OptionPath,
    right: &OptionPath,
    wildcard_match: impl Fn(&str, &str) -> bool,
) -> bool {
    let left = left.as_str().split('.');
    let right = right.as_str().split('.');
    left.clone().count() == right.clone().count()
        && left
            .zip(right)
            .all(|(left, right)| left == right || wildcard_match(left, right))
}

#[derive(Debug)]
pub struct ValueMismatch {
    pub pointer: String,
    pub expected: &'static str,
    pub actual: &'static str,
}

impl ValueMismatch {
    fn at(mut self, key: &str) -> Self {
        let token = key.replace('~', "~0").replace('/', "~1");
        self.pointer = format!("/{token}{}", self.pointer);
        self
    }
}

impl fmt::Display for ValueMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.pointer.is_empty() {
            write!(f, "at {:?} ", self.pointer)?;
        }
        write!(f, "expects {}; found {}", self.expected, self.actual)
    }
}

impl ValueType {
    pub(super) fn mismatch(&self, value: &Value) -> Option<ValueMismatch> {
        let expected = match self {
            Self::Primitive(kind) => kind.mismatch(value)?,
            Self::List { list } => {
                if let Some(values) = value.as_array() {
                    return values.iter().enumerate().find_map(|(index, value)| {
                        list.mismatch(value)
                            .map(|mismatch| mismatch.at(&index.to_string()))
                    });
                }
                "list"
            }
            Self::Map { map } => {
                if let Some(values) = value.as_object() {
                    return values.iter().find_map(|(key, value)| {
                        map.mismatch(value).map(|mismatch| mismatch.at(key))
                    });
                }
                "object"
            }
        };
        Some(ValueMismatch {
            pointer: String::new(),
            expected,
            actual: match value {
                Value::Null => "null",
                Value::Bool(_) => "boolean",
                Value::Number(_) => "number",
                Value::String(_) => "string",
                Value::Array(_) => "list",
                Value::Object(_) => "object",
            },
        })
    }

    fn has_source_file(&self) -> bool {
        match self {
            Self::Primitive(kind) => matches!(kind, PrimitiveType::SourceFile),
            Self::List { list } => list.has_source_file(),
            Self::Map { map } => map.has_source_file(),
        }
    }

    /// Compare validated values without confusing identical relative names from
    /// different source directories. The stored value and origin remain intact.
    pub(super) fn comparison_value<'a>(&self, value: &'a Value, origin: &Origin) -> Cow<'a, Value> {
        match self {
            Self::Primitive(PrimitiveType::SourceFile) => {
                let file = Path::new(origin.file());
                let directory = file.parent().unwrap_or(Path::new(""));
                let relative = value.as_str().expect("validated source file name");
                let joined = directory.join(relative);
                let path: PathBuf = joined.components().collect();
                Cow::Owned(Value::String(path.to_string_lossy().into_owned()))
            }
            Self::List { list } if list.has_source_file() => {
                let values = value.as_array().expect("validated list");
                Cow::Owned(Value::Array(
                    values
                        .iter()
                        .map(|value| list.comparison_value(value, origin).into_owned())
                        .collect(),
                ))
            }
            Self::Map { map } if map.has_source_file() => {
                let values = value.as_object().expect("validated object");
                Cow::Owned(Value::Object(
                    values
                        .iter()
                        .map(|(key, value)| {
                            (
                                key.clone(),
                                map.comparison_value(value, origin).into_owned(),
                            )
                        })
                        .collect(),
                ))
            }
            _ => Cow::Borrowed(value),
        }
    }
}

impl PrimitiveType {
    fn mismatch(&self, value: &Value) -> Option<&'static str> {
        match self {
            Self::Any => None,
            Self::Boolean => (!value.is_boolean()).then_some("boolean"),
            Self::String => (!value.is_string()).then_some("string"),
            Self::Integer => (!value.is_i64() && !value.is_u64()).then_some("integer"),
            Self::Number => (!value.is_number()).then_some("number"),
            Self::SourceFile => (!value.as_str().is_some_and(|path| {
                !path.is_empty() && !path.contains('\0') && Path::new(path).is_relative()
            }))
            .then_some("a relative source file path"),
        }
    }
}
