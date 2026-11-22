//! Versioned JSON frames shared by management and target processes.

use std::io::{self, BufRead, Read, Write};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use serde_json::Value;

use crate::{
    ContentDigest, InputObservation, Observation, PendingRunNotification, Plan, PlanMode,
    ResourceSet, RunStatus, TargetCapabilities, protocol::TargetQueryKind,
};

pub const TARGET_PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
/// Keep each content frame well below the protocol frame limit.
pub const CONTENT_CHUNK_BYTES: usize = 256 * 1024;

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum ClientFrame {
    Hello { versions: Vec<u32> },
    Request { id: u64, operation: TargetRequest },
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum ServerFrame {
    Hello { capabilities: TargetCapabilities },
    Incompatible { versions: Vec<u32> },
    Response { id: u64, result: TargetResult },
    Event { id: u64, event: TargetEvent },
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum TargetRequest {
    NormalizePath {
        path: String,
    },
    QueryInput {
        kind: TargetQueryKind,
        key: String,
    },
    PreparePlan {
        request: Box<PlanRequest>,
    },
    UploadContent {
        digest: ContentDigest,
        #[serde(with = "content_chunk")]
        chunk: Vec<u8>,
        complete: bool,
    },
    FinishPlan,
    ApplyPlan {
        host: ExecutionHost,
        #[serde(rename = "planId")]
        plan_id: String,
        #[serde(rename = "requestId")]
        request_id: String,
        interactive: bool,
    },
    ResumeRun {
        host: ExecutionHost,
        #[serde(rename = "runId")]
        run_id: String,
        interactive: bool,
    },
    RunStatus {
        host: ExecutionHost,
        query: RunQuery,
    },
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum TargetResult {
    Input {
        value: Value,
        observation: Observation,
    },
    NormalizedPath {
        original: String,
        normalized: String,
    },
    MissingContents {
        digests: Vec<ContentDigest>,
    },
    ContentAccepted,
    Planned(Box<RemotePlan>),
    ExecutionFinished(Box<RunStatus>),
    ExecutionStatus(Option<Box<RunStatus>>),
    ReviewRequired {
        reason: String,
        replacement: Option<Box<Plan>>,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlanRequest {
    pub repository_id: String,
    pub logical_host: String,
    pub resources: ResourceSet,
    pub used_inputs: Vec<InputObservation>,
    #[serde(default, skip_serializing_if = "is_normal_plan")]
    pub mode: PlanMode,
}

fn is_normal_plan(mode: &PlanMode) -> bool {
    *mode == PlanMode::Normal
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemotePlan {
    pub plan: Plan,
    pub pending_notifications: Vec<PendingRunNotification>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutionHost {
    pub repository_id: String,
    pub logical_host: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum RunQuery {
    Latest,
    Run(String),
    Request(String),
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum TargetEvent {
    RunStarted {
        #[serde(rename = "runId")]
        run_id: String,
    },
    TerminalRequired {
        token: String,
    },
}

/// Attachment IDs may appear in a fixed SSH command and a Unix socket name.
pub fn valid_terminal_token(token: &str) -> bool {
    token.len() == 32
        && token
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

mod content_chunk {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    use super::CONTENT_CHUNK_BYTES;

    pub(super) fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<u8>, D::Error> {
        let encoded = String::deserialize(deserializer)?;
        let bytes = STANDARD.decode(encoded).map_err(D::Error::custom)?;
        if bytes.len() > CONTENT_CHUNK_BYTES {
            return Err(D::Error::custom("oversized target content chunk"));
        }
        Ok(bytes)
    }
}

/// Read one complete frame, distinguishing clean EOF from a partial frame.
pub fn read_frame<T: DeserializeOwned>(input: &mut impl BufRead) -> io::Result<Option<T>> {
    let mut bytes = Vec::new();
    input
        .take((MAX_FRAME_BYTES + 1) as u64)
        .read_until(b'\n', &mut bytes)?;
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.len() > MAX_FRAME_BYTES || bytes.last() != Some(&b'\n') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "incomplete or oversized target protocol frame",
        ));
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Send and flush one frame before waiting for the peer.
pub fn write_frame(output: &mut impl Write, frame: &impl Serialize) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(frame)?;
    if bytes.len() >= MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "oversized target protocol frame",
        ));
    }
    bytes.push(b'\n');
    output.write_all(&bytes)?;
    output.flush()
}
