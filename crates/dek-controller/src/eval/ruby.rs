use std::{
    collections::HashSet,
    error::Error,
    fmt,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

use dek_core::{
    InputKind, InputObservation, Observation, Origin,
    protocol::{
        EVALUATOR_PROTOCOL_VERSION, EvalFailure, EvalOperation, EvalPayload, EvalRequest,
        InputQuery, TargetQuery, TargetQueryKind, TargetReply,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

const EVALUATOR: &str = concat!(
    include_str!("../../../../sdk/ruby/inputs.rb"),
    "\n",
    include_str!("../../../../sdk/ruby/declarations.rb"),
    "\n",
    include_str!("../../../../sdk/ruby/evaluator.rb"),
);

pub trait TargetProvider {
    fn query(&mut self, query: &TargetQuery) -> Result<TargetAnswer, String>;
}

/// Supplies an explicit HTTP input. `None` means no response is available.
pub trait HttpInputProvider {
    fn read(&mut self, url: &str) -> Result<Option<String>, String>;
}

pub struct RubyProviders<'a> {
    pub target: &'a mut dyn TargetProvider,
    pub http: Option<&'a mut dyn HttpInputProvider>,
}

impl RubyProviders<'_> {
    pub(super) fn reborrow(&mut self) -> RubyProviders<'_> {
        RubyProviders {
            target: &mut *self.target,
            http: self
                .http
                .as_mut()
                .map(|provider| &mut **provider as &mut dyn HttpInputProvider),
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum QueryKind {
    Target(TargetQueryKind),
    Http(HttpKind),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
enum HttpKind {
    Http,
}

#[derive(Debug)]
pub struct TargetAnswer {
    pub value: Value,
    pub observation: Observation,
}

#[derive(Debug)]
pub struct EvalOutput {
    pub id: String,
    pub result: Value,
    pub inputs: Vec<InputObservation>,
}

#[derive(Debug)]
pub struct RubyError {
    pub operation: EvalOperation,
    pub entry: PathBuf,
    pub origin: Option<Origin>,
    pub message: String,
}

impl fmt::Display for RubyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "Ruby {} ", self.operation)?;
        if let Some(origin) = &self.origin {
            write!(formatter, "{origin}")?;
        } else {
            write!(formatter, "{}", self.entry.display())?;
        }
        write!(formatter, ": {}", self.message)
    }
}

impl Error for RubyError {}

/// One evaluator session per controller run. The first request starts the child.
#[derive(Default)]
pub struct RubySession {
    process: Option<Process>,
    next_id: u64,
    failed: bool,
}

impl RubySession {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn evaluate(
        &mut self,
        operation: EvalOperation,
        entry: &Path,
        context: Value,
        target: &mut dyn TargetProvider,
    ) -> Result<EvalOutput, RubyError> {
        self.evaluate_request(
            operation,
            EvalPayload {
                entry,
                context: &context,
                repo_root: None,
            },
            RubyProviders { target, http: None },
        )
    }

    /// Enable the SDK and observe the request's entry, loaded code, and explicit inputs.
    /// Collect returns a UnitSpec, and expand preserves the script's result.
    pub fn evaluate_with_sdk(
        &mut self,
        operation: EvalOperation,
        repo_root: &Path,
        entry: &Path,
        context: Value,
        providers: RubyProviders<'_>,
    ) -> Result<EvalOutput, RubyError> {
        self.evaluate_request(
            operation,
            EvalPayload {
                entry,
                context: &context,
                repo_root: Some(repo_root),
            },
            providers,
        )
    }

    pub(super) fn invalidate(&mut self) {
        self.failed = true;
        self.process.take();
    }

    fn evaluate_request(
        &mut self,
        operation: EvalOperation,
        payload: EvalPayload<'_>,
        providers: RubyProviders<'_>,
    ) -> Result<EvalOutput, RubyError> {
        let entry = payload.entry;
        let error = |failure: EvalFailure| RubyError {
            operation,
            entry: entry.to_owned(),
            origin: failure.origin,
            message: failure.message,
        };
        if self.failed {
            return Err(error(failure("evaluator session has already failed")));
        }

        self.next_id += 1;
        let id = self.next_id.to_string();
        let request = EvalRequest {
            protocol_version: EVALUATOR_PROTOCOL_VERSION,
            id: &id,
            operation,
            payload,
        };
        let result = self.exchange(&request, providers);
        if result.is_err() {
            self.invalidate();
        }
        result.map_err(error)
    }

    fn exchange(
        &mut self,
        request: &EvalRequest<'_>,
        mut providers: RubyProviders<'_>,
    ) -> Result<EvalOutput, EvalFailure> {
        if self.process.is_none() {
            self.process = Some(Process::start(EVALUATOR)?);
        }
        let process = self.process.as_mut().expect("evaluator process started");
        process.send(request)?;

        let mut queries = HashSet::new();
        let mut inputs = Vec::new();
        loop {
            let frame = process.receive()?;
            if frame.get("parentId").is_some() {
                let query: InputQuery<QueryKind> = serde_json::from_value(frame)
                    .map_err(|err| failure(format!("invalid evaluator query: {err}")))?;
                if query.parent_id != request.id || !queries.insert(query.id.clone()) {
                    return Err(failure("unexpected target query request ID"));
                }
                let (kind, answer) = providers.answer(&query).map_err(|message| EvalFailure {
                    origin: query.origin.clone(),
                    message,
                })?;
                process.send(&TargetReply {
                    protocol_version: EVALUATOR_PROTOCOL_VERSION,
                    parent_id: request.id,
                    id: &query.id,
                    result: &answer.value,
                })?;
                inputs.push(InputObservation {
                    kind,
                    id: query.key,
                    observation: answer.observation,
                    origin: query.origin,
                });
                continue;
            }

            let result = response(frame, request.id, &mut inputs)?;
            return Ok(EvalOutput {
                id: request.id.to_owned(),
                result,
                inputs,
            });
        }
    }
}

impl RubyProviders<'_> {
    fn answer(
        &mut self,
        query: &InputQuery<QueryKind>,
    ) -> Result<(InputKind, TargetAnswer), String> {
        match query.kind {
            QueryKind::Target(kind) => {
                let answer = self.target.query(&TargetQuery {
                    protocol_version: query.protocol_version,
                    parent_id: query.parent_id.clone(),
                    id: query.id.clone(),
                    kind,
                    key: query.key.clone(),
                    origin: query.origin.clone(),
                })?;
                if kind == TargetQueryKind::Fact && answer.observation == Observation::Missing {
                    return Err(format!("target fact {:?} is unavailable", query.key));
                }
                Ok((kind.input_kind(), answer))
            }
            QueryKind::Http(HttpKind::Http) => {
                let url = &query.key;
                let response = match &mut self.http {
                    Some(http) => http
                        .read(url)
                        .map_err(|err| format!("HTTP input {url:?}: {err}"))?,
                    None => None,
                }
                .ok_or_else(|| format!("HTTP input {url:?} is unavailable; run dek refresh"))?;
                let observation = Observation::Digest(format!(
                    "sha256:{:x}",
                    Sha256::digest(response.as_bytes())
                ));
                Ok((
                    InputKind::Http,
                    TargetAnswer {
                        value: Value::String(response),
                        observation,
                    },
                ))
            }
        }
    }
}

fn response(
    mut frame: Value,
    id: &str,
    inputs: &mut Vec<InputObservation>,
) -> Result<Value, EvalFailure> {
    if frame.get("id").and_then(Value::as_str) != Some(id) {
        return Err(failure("unexpected evaluator response ID"));
    }
    match frame.get("ok").and_then(Value::as_bool) {
        Some(true) if frame.get("error").is_none() => {
            let object = frame
                .as_object_mut()
                .expect("response with an ID is an object");
            if let Some(observations) = object.remove("inputs") {
                let observations: Vec<InputObservation> = serde_json::from_value(observations)
                    .map_err(|err| {
                        failure(format!("invalid evaluator input observations: {err}"))
                    })?;
                inputs.extend(observations);
            }
            object
                .remove("result")
                .ok_or_else(|| failure("evaluator response has no result"))
        }
        Some(false) if frame.get("result").is_none() => {
            let error = serde_json::from_value(frame["error"].take())
                .map_err(|err| failure(format!("invalid evaluator error: {err}")))?;
            Err(error)
        }
        _ => Err(failure("invalid evaluator response")),
    }
}

struct Process {
    child: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
}

impl Process {
    fn start(script: &str) -> Result<Self, EvalFailure> {
        let mut child = Command::new("ruby")
            .args(["-e", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|err| failure(format!("cannot start ruby: {err}")))?;
        let input = child.stdin.take().expect("child stdin is piped");
        let output = BufReader::new(child.stdout.take().expect("child stdout is piped"));
        Ok(Self {
            child,
            input: Some(input),
            output,
        })
    }

    fn send(&mut self, frame: &impl Serialize) -> Result<(), EvalFailure> {
        let mut frame = serde_json::to_vec(frame)
            .map_err(|err| failure(format!("cannot encode evaluator request: {err}")))?;
        frame.push(b'\n');
        self.input
            .as_mut()
            .ok_or_else(|| failure("evaluator input is closed"))?
            .write_all(&frame)
            .map_err(|err| failure(format!("cannot write evaluator request: {err}")))
    }

    fn receive(&mut self) -> Result<Value, EvalFailure> {
        let mut frame = Vec::new();
        self.output
            .read_until(b'\n', &mut frame)
            .map_err(|err| failure(format!("cannot read evaluator response: {err}")))?;
        if frame.is_empty() {
            self.input.take();
            let status = self
                .child
                .wait()
                .map_err(|err| failure(format!("cannot wait for ruby: {err}")))?;
            return Err(failure(format!("evaluator exited with {status}")));
        }
        if frame.pop() != Some(b'\n') {
            return Err(failure("incomplete evaluator frame"));
        }
        let frame: Value = serde_json::from_slice(&frame)
            .map_err(|err| failure(format!("invalid evaluator JSON: {err}")))?;
        let version = frame.get("protocolVersion").and_then(Value::as_u64);
        if version != Some(u64::from(EVALUATOR_PROTOCOL_VERSION)) {
            return Err(failure(format!(
                "incompatible evaluator protocolVersion {}; expected {EVALUATOR_PROTOCOL_VERSION}",
                frame["protocolVersion"]
            )));
        }
        Ok(frame)
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn failure(message: impl Into<String>) -> EvalFailure {
    EvalFailure {
        message: message.into(),
        origin: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct UnusedTarget;

    impl TargetProvider for UnusedTarget {
        fn query(&mut self, _query: &TargetQuery) -> Result<TargetAnswer, String> {
            panic!("a malformed response must not query the target")
        }
    }

    #[test]
    fn closing_output_also_closes_the_request_pipe() {
        let mut process = Process::start(
            "require 'timeout'; STDIN.gets; STDOUT.reopen(File::NULL); Timeout.timeout(2) { STDIN.read }; exit! 23",
        )
        .unwrap();
        process.send(&Value::Null).unwrap();
        let error = process.receive().unwrap_err();
        assert!(error.message.contains("23"), "{error:?}");
    }

    #[test]
    fn malformed_frames_invalidate_the_session() {
        for (frame, expected) in [
            (
                r#"{"protocolVersion":2,"id":"1","ok":true,"result":true}"#,
                "incompatible evaluator protocolVersion 2",
            ),
            ("not JSON", "invalid evaluator JSON"),
            (
                r#"{"protocolVersion":1,"parentId":"unexpected","id":"1","kind":"fact","key":"architecture"}"#,
                "unexpected target query request ID",
            ),
            (
                r#"{"protocolVersion":1,"id":"unexpected","ok":true,"result":true}"#,
                "unexpected evaluator response ID",
            ),
            (
                r#"{"protocolVersion":1,"id":"1","ok":true}"#,
                "evaluator response has no result",
            ),
        ] {
            let script = format!("STDIN.gets\nSTDOUT.puts {}", serde_json::json!(frame));
            let mut session = RubySession {
                process: Some(Process::start(&script).unwrap()),
                ..RubySession::default()
            };
            let entry = Path::new("fixtures/bad.rb");
            let error = session
                .evaluate(EvalOperation::Expand, entry, Value::Null, &mut UnusedTarget)
                .unwrap_err()
                .to_string();
            assert!(error.contains("expand fixtures/bad.rb"), "{error}");
            assert!(error.contains(expected), "{error}");
            assert!(session.process.is_none());
            let retry = session
                .evaluate(EvalOperation::Expand, entry, Value::Null, &mut UnusedTarget)
                .unwrap_err();
            assert!(retry.message.contains("already failed"));
        }
    }
}
