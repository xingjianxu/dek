//! Target-owned reviewed plans and content-addressed objects.

mod fs;
mod runs;

pub use runs::{ActionAttempt, ActionReceipt, ActionResult, RunAction, RunNotification, RunRecord};

use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{self, Cursor},
    marker::PhantomData,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use dek_core::{ContentDigest, Plan, ResourceSet, TargetIdentity};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use self::fs::Directory;
use crate::planner::{plan_id, resource_digest};

pub const DEFAULT_ROOT: &str = "/var/lib/dek";
const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum StateError {
    #[error("无法{action} {path}：{source}。请检查目标状态目录后重新运行 dek plan")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("目标状态 {path}：{message}。请在正确的目标主机和仓库重新运行 dek plan")]
    Invalid { path: PathBuf, message: String },
}

impl StateError {
    fn io(action: &'static str, path: &Path, source: io::Error) -> Self {
        Self::Io {
            action,
            path: path.into(),
            source,
        }
    }

    fn invalid(path: &Path, message: impl Into<String>) -> Self {
        Self::Invalid {
            path: path.into(),
            message: message.into(),
        }
    }
}

/// The trusted target account owns the state root and its children.
pub struct StateStore {
    root: Directory,
    plans: Directory,
    objects: Directory,
}

/// A target-wide transaction. A later state consumer can pin a plan reference
/// and read its complete content under the same lock.
/// Threads doing independent updates must each acquire their own guard.
pub struct StateGuard<'a> {
    store: &'a StateStore,
    _lock: File,
    _single_thread: PhantomData<Cell<()>>,
}

/// Serializes execution in this target state root while short state transactions
/// remain independently accessible to status and retention.
pub(crate) struct ExecutionLease {
    _lock: File,
}

#[derive(Clone, Copy)]
pub struct PlanBinding<'a> {
    pub repository_id: &'a str,
    pub logical_host: &'a str,
    pub target: &'a TargetIdentity,
}

#[derive(Debug)]
pub struct ReviewedPlan {
    pub logical_host: String,
    pub plan: Plan,
    pub resources: ResourceSet,
    pub contents: BTreeMap<ContentDigest, Vec<u8>>,
    /// Unix seconds when this plan was first saved.
    pub created_at: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Record<P, R, H> {
    format_version: u32,
    logical_host: H,
    created_at: u64,
    plan: P,
    resources: R,
}

type OwnedRecord = Record<Plan, ResourceSet, String>;

impl StateStore {
    pub(crate) fn execution_lease(&self) -> Result<ExecutionLease, StateError> {
        Ok(ExecutionLease {
            _lock: self.root.lock("execution.lock")?,
        })
    }

    pub fn open(path: &Path) -> Result<Self, StateError> {
        let root = Directory::open_root(path)?;
        let plans = root.private_child("plans")?;
        let objects = root.private_child("objects")?;
        Ok(Self {
            root,
            plans,
            objects,
        })
    }

    pub fn lock(&self) -> Result<StateGuard<'_>, StateError> {
        let lock = self.root.lock("state.lock")?;
        Ok(StateGuard {
            store: self,
            _lock: lock,
            _single_thread: PhantomData,
        })
    }
}

impl StateGuard<'_> {
    pub fn has_object(&self, digest: &ContentDigest) -> Result<bool, StateError> {
        self.object(digest).map(|object| object.is_some())
    }

    pub fn read_object(&self, digest: &ContentDigest) -> Result<Vec<u8>, StateError> {
        self.object(digest)?.ok_or_else(|| {
            StateError::invalid(
                &self.store.objects.path,
                format!("缺少内容对象 {}", digest.0),
            )
        })
    }

    pub fn put_object(&self, digest: &ContentDigest, bytes: &[u8]) -> Result<(), StateError> {
        let key = self.object_key(digest)?;
        self.verify_bytes(key, bytes)?;
        if self.object(digest)?.is_none() {
            self.store.objects.publish(key, bytes)?;
        }
        Ok(())
    }

    pub fn save_plan(
        &self,
        logical_host: &str,
        plan: &Plan,
        resources: &ResourceSet,
        supplied: &BTreeMap<ContentDigest, Vec<u8>>,
    ) -> Result<(), StateError> {
        let name = self.plan_name(&plan.id)?;
        let path = self.store.plans.path.join(&name);
        self.validate_record(&path, &plan.id, logical_host, plan, resources)?;
        let mut pending = Vec::new();
        for digest in referenced(plan, resources) {
            let bytes = supplied.get(digest);
            let key = self.object_key(digest)?;
            if let Some(bytes) = bytes {
                self.verify_bytes(key, bytes)?;
            }
            if self.object(digest)?.is_none() {
                let bytes = bytes.ok_or_else(|| {
                    StateError::invalid(&path, format!("缺少内容对象 {}", digest.0))
                })?;
                pending.push((key, bytes));
            }
        }

        let prior = self.load_record(&name)?;
        if let Some(record) = &prior {
            // Reuse the prior publication only if it has the same reviewed payload.
            if record.plan != *plan
                || record.resources != *resources
                || record.logical_host != logical_host
            {
                return Err(StateError::invalid(&path, "已有同 ID 计划的内容不同"));
            }
        }

        for (key, bytes) in pending {
            self.store.objects.publish(key, bytes)?;
        }
        self.store.objects.sync()?;
        if prior.is_some() {
            return self.store.plans.sync();
        }
        let record = Record {
            format_version: FORMAT_VERSION,
            logical_host,
            created_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            plan,
            resources,
        };
        let data = rmp_serde::to_vec_named(&record)
            .map_err(|err| StateError::invalid(&path, format!("无法编码计划：{err}")))?;
        self.store.plans.publish(&name, &data)
    }

    pub fn load_plan(
        &self,
        id: &str,
        binding: PlanBinding<'_>,
    ) -> Result<ReviewedPlan, StateError> {
        let name = self.plan_name(id)?;
        let path = self.store.plans.path.join(&name);
        let record = self
            .load_record(&name)?
            .ok_or_else(|| StateError::invalid(&path, "缺少已审阅计划"))?;
        if record.plan.repository_id != binding.repository_id {
            return Err(StateError::invalid(
                &path,
                format!(
                    "计划仓库 {} 与当前仓库 {} 不符",
                    record.plan.repository_id, binding.repository_id
                ),
            ));
        }
        if record.logical_host != binding.logical_host || record.plan.target != *binding.target {
            return Err(StateError::invalid(
                &path,
                format!(
                    "计划主机 {}（{}）与当前主机 {}（{}）不符",
                    record.logical_host,
                    record.plan.target.id,
                    binding.logical_host,
                    binding.target.id
                ),
            ));
        }
        self.validate_record(
            &path,
            id,
            &record.logical_host,
            &record.plan,
            &record.resources,
        )?;
        let contents = referenced(&record.plan, &record.resources)
            .into_iter()
            .map(|digest| {
                self.read_object(digest)
                    .map(|bytes| (digest.clone(), bytes))
            })
            .collect::<Result<_, _>>()?;
        Ok(ReviewedPlan {
            logical_host: record.logical_host,
            plan: record.plan,
            resources: record.resources,
            contents,
            created_at: record.created_at,
        })
    }

    fn plan_name(&self, id: &str) -> Result<String, StateError> {
        if !valid_hash(id) {
            return Err(StateError::invalid(
                &self.store.plans.path,
                format!("无效计划 ID {id:?}"),
            ));
        }
        Ok(format!("{id}.mpk"))
    }

    fn object_key<'a>(&self, digest: &'a ContentDigest) -> Result<&'a str, StateError> {
        digest
            .0
            .strip_prefix("sha256:")
            .filter(|key| valid_hash(key))
            .ok_or_else(|| {
                StateError::invalid(
                    &self.store.objects.path,
                    format!("无效内容摘要 {:?}", digest.0),
                )
            })
    }

    fn verify_bytes(&self, key: &str, bytes: &[u8]) -> Result<(), StateError> {
        if format!("{:x}", Sha256::digest(bytes)) != key {
            return Err(StateError::invalid(
                &self.store.objects.path.join(key),
                format!("内容对象 sha256:{key} 与实际字节摘要不符"),
            ));
        }
        Ok(())
    }

    fn object(&self, digest: &ContentDigest) -> Result<Option<Vec<u8>>, StateError> {
        let key = self.object_key(digest)?;
        let bytes = self.store.objects.read(key)?;
        if let Some(bytes) = &bytes {
            self.verify_bytes(key, bytes)?;
        }
        Ok(bytes)
    }

    fn load_record(&self, name: &str) -> Result<Option<OwnedRecord>, StateError> {
        let Some(bytes) = self.store.plans.read(name)? else {
            return Ok(None);
        };
        let path = self.store.plans.path.join(name);
        let mut cursor = Cursor::new(&bytes);
        let record: OwnedRecord = rmp_serde::from_read(&mut cursor)
            .map_err(|err| StateError::invalid(&path, format!("无法解码计划：{err}")))?;
        if cursor.position() != bytes.len() as u64 {
            return Err(StateError::invalid(&path, "计划记录含额外字节"));
        }
        if record.format_version != FORMAT_VERSION {
            return Err(StateError::invalid(
                &path,
                format!("不支持的计划记录版本 {}", record.format_version),
            ));
        }
        Ok(Some(record))
    }

    fn validate_record(
        &self,
        path: &Path,
        id: &str,
        logical_host: &str,
        plan: &Plan,
        resources: &ResourceSet,
    ) -> Result<(), StateError> {
        let encode = |err| StateError::invalid(path, format!("无法计算计划摘要：{err}"));
        if plan.id != id
            || plan_id(plan, Some(logical_host)).map_err(encode)? != id
            || resource_digest(resources).map_err(encode)? != plan.resource_digest
        {
            return Err(StateError::invalid(
                path,
                "计划 ID 或资源摘要与审阅内容不符",
            ));
        }
        Ok(())
    }
}

fn referenced<'a>(plan: &'a Plan, resources: &'a ResourceSet) -> BTreeSet<&'a ContentDigest> {
    let mut digests = resources.content_digests();
    digests.extend(plan.content_digests());
    digests
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}
