use std::{collections::BTreeMap, ffi::OsStr, fmt, fs};

use dek_core::{
    InputKind, InputObservation, Observation, OptionAssignment, OptionPath, Origin, ProfileRef,
    UnitSpec,
};
use serde::{
    Deserialize, Deserializer,
    de::{MapAccess, Visitor, value::MapAccessDeserializer},
};
use serde_json::Value;
use serde_path_to_error::Segment;
use sha2::{Digest, Sha256};

use super::{CollectError, CollectRequest, Collector};
use crate::json_object::Object;

const SCHEMA_VERSION: u64 = 1;

/// 静态 JSON 采集器。
#[derive(Debug, Default)]
pub struct JsonCollector;

impl Collector for JsonCollector {
    fn collect(&mut self, request: &CollectRequest<'_>) -> Result<UnitSpec, CollectError> {
        let path = request.entry_path();
        let source = JsonSource(request.entry);
        if path.extension() != Some(OsStr::new("json")) {
            return Err(source.error("", "静态 JSON 采集需要 .json 入口"));
        }
        let contents =
            fs::read(&path).map_err(|err| source.error("", format!("无法读取入口文件：{err}")))?;
        let Object(header): Object<Header> = source.decode(&contents)?;
        if header.schema_version != SCHEMA_VERSION {
            return Err(source.error(
                "/schemaVersion",
                format!(
                    "不支持的 schemaVersion {}（期望 {SCHEMA_VERSION}）",
                    header.schema_version
                ),
            ));
        }
        let Object(unit): Object<JsonUnit> = source.decode(&contents)?;

        let imports = unit
            .imports
            .into_iter()
            .enumerate()
            .map(|(index, JsonImport(import))| {
                let pointer = format!("/imports/{index}");
                if import.name.trim().is_empty() || import.name.chars().any(char::is_control) {
                    return Err(source.error(&pointer, "需要有效的 profile 名称"));
                }
                Ok(ProfileRef {
                    name: import.name,
                    params: import.params,
                    origin: Some(source.origin(&pointer)),
                })
            })
            .collect::<Result<_, _>>()?;
        let options = unit
            .options
            .into_iter()
            .enumerate()
            .map(|(index, Object(option))| OptionAssignment {
                path: option.path,
                value: option.value,
                origin: source.origin(&format!("/options/{index}")),
            })
            .collect();
        let inputs = vec![InputObservation {
            kind: InputKind::File,
            id: request.entry.to_owned(),
            observation: Observation::Digest(format!("sha256:{:x}", Sha256::digest(contents))),
            origin: None,
        }];
        Ok(UnitSpec {
            imports,
            options,
            inputs,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Header {
    schema_version: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonUnit {
    #[serde(rename = "schemaVersion")]
    _schema_version: u64,
    imports: Vec<JsonImport>,
    options: Vec<Object<JsonOption>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonImportFields {
    name: String,
    #[serde(default)]
    params: BTreeMap<String, Value>,
}

struct JsonImport(JsonImportFields);

impl<'de> Deserialize<'de> for JsonImport {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ImportVisitor;

        impl<'de> Visitor<'de> for ImportVisitor {
            type Value = JsonImport;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a profile name or import object")
            }

            fn visit_str<E: serde::de::Error>(self, name: &str) -> Result<Self::Value, E> {
                Ok(JsonImport(JsonImportFields {
                    name: name.to_owned(),
                    params: BTreeMap::new(),
                }))
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                JsonImportFields::deserialize(MapAccessDeserializer::new(map)).map(JsonImport)
            }
        }

        deserializer.deserialize_any(ImportVisitor)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonOption {
    path: OptionPath,
    value: Value,
}

struct JsonSource<'a>(&'a str);

impl JsonSource<'_> {
    fn origin(&self, pointer: &str) -> Origin {
        Origin::Json {
            file: self.0.to_owned(),
            pointer: pointer.parse().expect("采集器生成的 JSON Pointer 有效"),
        }
    }

    fn error(&self, pointer: &str, message: impl Into<String>) -> CollectError {
        CollectError {
            origin: Some(self.origin(pointer)),
            message: message.into(),
        }
    }

    fn decode<'de, T: Deserialize<'de>>(&self, contents: &'de [u8]) -> Result<T, CollectError> {
        let mut decoder = serde_json::Deserializer::from_slice(contents);
        let value = serde_path_to_error::deserialize(&mut decoder)
            .map_err(|err| self.error(&json_pointer(err.path()), err.inner().to_string()))?;
        decoder
            .end()
            .map_err(|err| self.error("", err.to_string()))?;
        Ok(value)
    }
}

fn json_pointer(path: &serde_path_to_error::Path) -> String {
    let mut pointer = String::new();
    for segment in path {
        pointer.push('/');
        match segment {
            Segment::Seq { index } => pointer.push_str(&index.to_string()),
            Segment::Map { key } | Segment::Enum { variant: key } => {
                pointer.push_str(&key.replace('~', "~0").replace('/', "~1"));
            }
            Segment::Unknown => break,
        }
    }
    pointer
}
