use std::{error::Error, fmt, num::NonZeroU32, str::FromStr};

use serde::{Deserialize, Serialize};

/// The location of a declaration or input access in a repository source file.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged, from = "OriginWire")]
pub enum Origin {
    Ruby { file: String, line: NonZeroU32 },
    Json { file: String, pointer: JsonPointer },
}

// The separate structs accept named fields and positional MessagePack fields.
#[derive(Deserialize)]
#[serde(untagged)]
enum OriginWire {
    Ruby(RubyOriginFields),
    Json(JsonOriginFields),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RubyOriginFields {
    file: String,
    line: NonZeroU32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonOriginFields {
    file: String,
    pointer: JsonPointer,
}

impl From<OriginWire> for Origin {
    fn from(origin: OriginWire) -> Self {
        match origin {
            OriginWire::Ruby(RubyOriginFields { file, line }) => Self::Ruby { file, line },
            OriginWire::Json(JsonOriginFields { file, pointer }) => Self::Json { file, pointer },
        }
    }
}

impl Origin {
    /// The recorded source file path, with its original spelling.
    pub fn file(&self) -> &str {
        match self {
            Self::Ruby { file, .. } | Self::Json { file, .. } => file,
        }
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ruby { file, line } => write!(f, "{file}:{line}"),
            Self::Json { file, pointer } => {
                write!(f, "{file} (JSON Pointer {:?})", pointer.as_str())
            }
        }
    }
}

/// A JSON Pointer in the string representation defined by RFC 6901.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "String")]
pub struct JsonPointer(String);

impl JsonPointer {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for JsonPointer {
    type Error = InvalidJsonPointer;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let valid_start = value.is_empty() || value.starts_with('/');
        let valid_escapes = value
            .split('~')
            .skip(1)
            .all(|suffix| suffix.starts_with(['0', '1']));

        if valid_start && valid_escapes {
            Ok(Self(value))
        } else {
            Err(InvalidJsonPointer(value))
        }
    }
}

impl FromStr for JsonPointer {
    type Err = InvalidJsonPointer;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value.to_owned().try_into()
    }
}

impl fmt::Display for JsonPointer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InvalidJsonPointer(String);

impl fmt::Display for InvalidJsonPointer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid JSON Pointer {:?}", self.0)
    }
}

impl Error for InvalidJsonPointer {}
