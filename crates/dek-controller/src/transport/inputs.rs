use dek_core::{
    Observation,
    protocol::{TargetQuery, TargetQueryKind},
    target_protocol::{TargetRequest, TargetResult},
};
use serde_json::json;

use crate::{
    cache::{CacheEntry, CacheStore, UnitKey, UnitStage, semantic_digest},
    eval::{TargetAnswer, TargetProvider, TargetSnapshotSource},
};

use super::{SshError, SshSession};

const SNAPSHOT_SIGNATURE: &str = "target-input-v1";
type RecordedInput = Result<TargetAnswer, String>;

/// Live target input queries over a checked SSH connection.
pub struct RemoteTarget<'session, 'cache> {
    connection: Connection<'session>,
    snapshot: Option<TargetInputSnapshot<'cache>>,
    binding: Binding,
}

enum Connection<'a> {
    Borrowed(&'a mut SshSession),
    OnDemand(Option<Box<SshSession>>),
}

struct Binding {
    logical_host: String,
    alias: String,
    target_id: String,
}

impl Binding {
    fn new(logical_host: &str, alias: &str, target_id: &str) -> Self {
        Self {
            logical_host: logical_host.into(),
            alias: alias.into(),
            target_id: target_id.into(),
        }
    }

    fn label(&self) -> String {
        format!(
            "主机 {:?}，SSH 别名 {:?}，目标 {:?}",
            self.logical_host, self.alias, self.target_id,
        )
    }

    fn key(&self) -> String {
        json!([self.logical_host, self.alias, self.target_id]).to_string()
    }
}

impl<'session, 'cache> RemoteTarget<'session, 'cache> {
    /// Reuse the verified session that will also carry the remote plan request.
    pub fn new(
        session: &'session mut SshSession,
        cache: Option<&'cache CacheStore>,
        logical_host: &str,
    ) -> Self {
        let binding = Binding::new(
            logical_host,
            &session.alias,
            &session.capabilities.target.id,
        );
        Self {
            connection: Connection::Borrowed(session),
            snapshot: cache.map(|store| TargetInputSnapshot::for_binding(store, &binding)),
            binding,
        }
    }

    /// Read-only evaluation can avoid connecting if it uses no target inputs.
    pub fn on_demand(
        cache: Option<&'cache CacheStore>,
        logical_host: &str,
        alias: &str,
        target_id: &str,
    ) -> Self {
        let binding = Binding::new(logical_host, alias, target_id);
        Self {
            connection: Connection::OnDemand(None),
            snapshot: cache.map(|store| TargetInputSnapshot::for_binding(store, &binding)),
            binding,
        }
    }

    pub fn finish(self) -> Result<(), SshError> {
        if let Connection::OnDemand(Some(session)) = self.connection {
            session.finish()?;
        }
        Ok(())
    }
}

impl TargetProvider for RemoteTarget<'_, '_> {
    fn query(&mut self, query: &TargetQuery) -> Result<TargetAnswer, String> {
        let fail = |message: &str| {
            format!(
                "{} 的目标输入 {:?} {:?}：{message}",
                self.binding.label(),
                query.kind,
                query.key
            )
        };
        let session = match &mut self.connection {
            Connection::Borrowed(session) => &mut **session,
            Connection::OnDemand(session) => {
                if session.is_none() {
                    *session = Some(Box::new(
                        SshSession::connect(&self.binding.alias, &self.binding.target_id)
                            .map_err(|error| fail(&error.to_string()))?,
                    ));
                }
                session.as_deref_mut().expect("SSH session is connected")
            }
        };
        let previous = self
            .snapshot
            .as_ref()
            .map(|snapshot| {
                snapshot
                    .store
                    .lookup::<RecordedInput>(&snapshot.key(query), SNAPSHOT_SIGNATURE)
            })
            .transpose()
            .map_err(|error| fail(&error.to_string()))?;
        let result = session
            .request(TargetRequest::QueryInput {
                kind: query.kind,
                key: query.key.clone(),
            })
            .map_err(|error| fail(&error.to_string()))?;
        let answer = match result {
            TargetResult::Input { value, observation } => Ok(TargetAnswer { value, observation }),
            TargetResult::Error { message } => Err(message),
            _ => return Err(fail("目标协议返回了其他请求的结果")),
        };

        if let (Some(snapshot), Some(previous)) = (&self.snapshot, previous)
            && previous.entry.as_ref().map(|entry| &entry.output) != Some(&answer)
        {
            snapshot
                .store
                .publish(
                    &previous,
                    &CacheEntry {
                        dependencies: Vec::new(),
                        semantic_digest: semantic_digest(&json!(answer)),
                        output: answer.clone(),
                    },
                    |_| Ok(true),
                )
                .map_err(|error| fail(&error.to_string()))?;
        }
        required_fact(query, answer).map_err(|message| fail(&message))
    }
}

/// Recorded answers from one configured and previously verified SSH binding.
pub struct TargetInputSnapshot<'a> {
    store: &'a CacheStore,
    scope: String,
    source: TargetSnapshotSource,
}

impl<'a> TargetInputSnapshot<'a> {
    pub fn new(store: &'a CacheStore, logical_host: &str, alias: &str, target_id: &str) -> Self {
        Self::for_binding(store, &Binding::new(logical_host, alias, target_id))
    }

    fn for_binding(store: &'a CacheStore, binding: &Binding) -> Self {
        Self {
            store,
            scope: binding.key(),
            source: TargetSnapshotSource {
                label: binding.label(),
            },
        }
    }

    fn key(&self, query: &TargetQuery) -> UnitKey {
        UnitKey {
            stage: UnitStage::TargetSnapshot,
            unit: json!([query.kind, query.key]).to_string(),
            params: Default::default(),
            target: Some(self.scope.clone()),
        }
    }
}

impl TargetProvider for TargetInputSnapshot<'_> {
    fn query(&mut self, query: &TargetQuery) -> Result<TargetAnswer, String> {
        let fail = |message: &str| {
            format!(
                "目标快照（{}）中的 {:?} {:?}：{message}",
                self.source.label, query.kind, query.key,
            )
        };
        let entry = self
            .store
            .lookup::<RecordedInput>(&self.key(query), SNAPSHOT_SIGNATURE)
            .map_err(|error| fail(&error.to_string()))?
            .entry
            .ok_or_else(|| fail("没有已记录的观测，请先连接目标查看配置"))?;
        required_fact(query, entry.output).map_err(|message| fail(&message))
    }

    fn snapshot_source(&self) -> Option<&TargetSnapshotSource> {
        Some(&self.source)
    }
}

fn required_fact(query: &TargetQuery, result: RecordedInput) -> RecordedInput {
    let answer = result?;
    if query.kind == TargetQueryKind::Fact && answer.observation == Observation::Missing {
        return Err("target fact is unavailable".into());
    }
    Ok(answer)
}
