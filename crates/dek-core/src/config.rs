use std::{collections::BTreeMap, error::Error, fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Origin;

/// A case-sensitive option path with nonempty, dot-separated components.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(try_from = "String")]
pub struct OptionPath(String);

impl OptionPath {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for OptionPath {
    type Error = InvalidOptionPath;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.split('.').any(str::is_empty) || value.chars().any(char::is_control) {
            Err(InvalidOptionPath(value))
        } else {
            Ok(Self(value))
        }
    }
}

impl FromStr for OptionPath {
    type Err = InvalidOptionPath;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value.to_owned().try_into()
    }
}

impl fmt::Display for OptionPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvalidOptionPath(String);

impl fmt::Display for InvalidOptionPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid option path {:?}", self.0)
    }
}

impl Error for InvalidOptionPath {}

/// Declarations collected from one evaluation unit, in declaration order.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct UnitSpec {
    pub imports: Vec<ProfileRef>,
    pub options: Vec<OptionAssignment>,
    pub inputs: Vec<InputObservation>,
}

/// A profile reference with named JSON parameters and its declaration location.
///
/// A bare profile name is also accepted on input, with empty parameters and no location.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(from = "ProfileRefWire")]
pub struct ProfileRef {
    pub name: String,
    pub params: BTreeMap<String, Value>,
    pub origin: Option<Origin>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ProfileRefWire {
    Name(String),
    Detailed(ProfileRefFields),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileRefFields {
    name: String,
    #[serde(default)]
    params: BTreeMap<String, Value>,
    origin: Option<Origin>,
}

impl From<ProfileRefWire> for ProfileRef {
    fn from(reference: ProfileRefWire) -> Self {
        match reference {
            ProfileRefWire::Name(name) => Self {
                name,
                params: BTreeMap::new(),
                origin: None,
            },
            ProfileRefWire::Detailed(ProfileRefFields {
                name,
                params,
                origin,
            }) => Self {
                name,
                params,
                origin,
            },
        }
    }
}

/// An option value as collected, before the controller assigns its priority.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct OptionAssignment {
    pub path: OptionPath,
    pub value: Value,
    pub origin: Origin,
}

/// One input read during evaluation, identified within its input kind.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct InputObservation {
    pub kind: InputKind,
    pub id: String,
    pub observation: Observation,
    pub origin: Option<Origin>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum InputKind {
    /// Includes the evaluation entry, loaded code, and other local file reads.
    File,
    Directory,
    Environment,
    TargetFact,
    TargetCapability,
    TargetFile,
    Http,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Observation {
    Digest(String),
    Missing,
}

/// Options after schema validation and merging by the controller.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct HostConfig {
    pub options: BTreeMap<OptionPath, EffectiveOption>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectiveOption {
    pub value: Value,
    pub priority: Priority,
    pub effective_definitions: Vec<OptionDefinition>,
    pub overridden_definitions: Vec<OptionDefinition>,
}

/// The original assignment for an option, with its merge priority.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct OptionDefinition {
    pub value: Value,
    pub origin: Origin,
    pub priority: Priority,
}

/// Merge priority, ordered from lowest to highest.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Priority {
    ModuleDefault,
    Profile,
    Host,
}
