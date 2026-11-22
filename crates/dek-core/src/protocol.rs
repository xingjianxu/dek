//! Wire types for version 1 of the language evaluator protocol.

use std::{fmt, path::Path};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{InputKind, Origin};

pub const EVALUATOR_PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum EvalOperation {
    Collect,
    Expand,
}

impl fmt::Display for EvalOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Collect => "collect",
            Self::Expand => "expand",
        })
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvalRequest<'a> {
    pub protocol_version: u32,
    pub id: &'a str,
    pub operation: EvalOperation,
    pub payload: EvalPayload<'a>,
}

#[derive(Debug, Serialize)]
pub struct EvalPayload<'a> {
    pub entry: &'a Path,
    pub context: &'a Value,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TargetQuery {
    pub protocol_version: u32,
    pub parent_id: String,
    pub id: String,
    pub kind: TargetQueryKind,
    pub key: String,
    pub origin: Option<Origin>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum TargetQueryKind {
    Fact,
    Capability,
    ReadText,
}

impl TargetQueryKind {
    pub fn input_kind(self) -> InputKind {
        match self {
            Self::Fact => InputKind::TargetFact,
            Self::Capability => InputKind::TargetCapability,
            Self::ReadText => InputKind::TargetFile,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetReply<'a> {
    pub protocol_version: u32,
    pub parent_id: &'a str,
    pub id: &'a str,
    pub result: &'a Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalFailure {
    pub message: String,
    pub origin: Option<Origin>,
}
