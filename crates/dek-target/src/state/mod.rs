//! Target-owned reviewed plans and content-addressed objects.

mod fs;
mod remote_plan;
mod retention;
mod runs;

pub use dek_core::{ActionResult, PendingRunNotification, RunAction, RunNotification};
pub use remote_plan::PlanSession;
pub use runs::{ActionAttempt, ActionReceipt, RunRecord};

use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{self, Cursor},
    marker::PhantomData,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use dek_core::{ContentDigest, Plan, PlanMode, ResourceSet, TargetIdentity};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use self::{fs::Directory, retention::RetentionPolicy};
use crate::planner::{plan_id, resource_digest};

pub const DEFAULT_ROOT: &str = "/var/lib/dek";
const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum StateError {
    #[error("计划 {id} 已于 Unix 时间 {expired_at} 过期。请重新运行 dek plan 并审阅新计划")]
    ExpiredPlan {
        id: String,
        expired_at: u64,
        mode: PlanMode,
    },
    #[error("缺少已审阅计划 {id}（{path}）。请重新运行 dek plan")]
    MissingPlan { id: String, path: PathBuf },
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
    expired: Directory,
    clock: Box<dyn Fn() -> u64 + Send + Sync>,
}

/// Secret verification material is read locally and never enters a plan record.
pub struct SecretVerification {
    pub key: Vec<u8>,
    pub marker: Vec<u8>,
}

/// A target-wide transaction. A later state consumer can pin a plan reference
/// and read its complete content under the same lock.
/// Threads doing independent updates must each acquire their own guard.
pub struct StateGuard<'a> {
    store: &'a StateStore,
    _lock: File,
    _single_thread: PhantomData<Cell<()>>,
    retention: RetentionPolicy,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    issuance: Option<u64>,
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

    /// Read a marker from the existing private state hierarchy without initializing it.
    pub fn read_secret_verification(
        path: &Path,
        id: &str,
    ) -> Result<Option<SecretVerification>, StateError> {
        Self::read_secret_marker(path, id, "marker")
    }

    /// Read publication's protected verification checkpoint, if one exists.
    pub fn read_secret_candidate(
        path: &Path,
        id: &str,
    ) -> Result<Option<SecretVerification>, StateError> {
        Self::read_secret_marker(path, id, "candidate")
    }

    fn read_secret_marker(
        path: &Path,
        id: &str,
        suffix: &str,
    ) -> Result<Option<SecretVerification>, StateError> {
        if !valid_hash(id) {
            return Err(StateError::invalid(path, "无效的验证标记 ID"));
        }
        let Some(root) = Directory::existing_root(path)? else {
            return Ok(None);
        };
        let Some(secrets) = root.existing_private_child("secrets")? else {
            return Ok(None);
        };
        let Some(marker) = secrets.read(&format!("{id}.{suffix}"))? else {
            return Ok(None);
        };
        Ok(secrets
            .read("verification.key")?
            .map(|key| SecretVerification { key, marker }))
    }

    pub fn open(path: &Path) -> Result<Self, StateError> {
        Self::open_with_clock(path, || {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
        })
    }

    /// Use an explicit Unix-seconds clock for the target plan lifecycle.
    pub fn open_with_clock(
        path: &Path,
        clock: impl Fn() -> u64 + Send + Sync + 'static,
    ) -> Result<Self, StateError> {
        let root = Directory::open_root(path)?;
        let plans = root.private_child("plans")?;
        let objects = root.private_child("objects")?;
        let expired = root.private_child("expired")?;
        Ok(Self {
            root,
            plans,
            objects,
            expired,
            clock: Box::new(clock),
        })
    }

    pub fn lock(&self) -> Result<StateGuard<'_>, StateError> {
        let lock = self.root.lock("state.lock")?;
        let retention = RetentionPolicy::read(&self.root)?;
        Ok(StateGuard {
            store: self,
            _lock: lock,
            _single_thread: PhantomData,
            retention,
        })
    }
}

impl StateGuard<'_> {
    /// Return an existing target-local key or initialize it once under the state lock.
    pub fn secret_verification_key(&self) -> Result<[u8; 32], StateError> {
        use std::{fs::File, io::Read};

        let secrets = self.store.root.private_child("secrets")?;
        let name = "verification.key";
        if let Some(key) = secrets.read(name)? {
            return key
                .try_into()
                .map_err(|_| StateError::invalid(&secrets.path.join(name), "验证密钥长度不符"));
        }
        let mut key = [0; 32];
        let device = Path::new("/dev/urandom");
        File::open(device)
            .and_then(|mut file| file.read_exact(&mut key))
            .map_err(|err| StateError::io("生成验证密钥", device, err))?;
        secrets.publish(name, &key)?;
        Ok(key)
    }

    /// Persist the verified bytes' marker before their atomic publication.
    pub fn stage_secret_verification(&self, id: &str, marker: &[u8]) -> Result<(), StateError> {
        self.secret_marker_directory(id)?
            .replace(&format!("{id}.candidate"), marker)
    }

    /// Certify the destination and clear its publication checkpoint.
    pub fn write_secret_verification(&self, id: &str, marker: &[u8]) -> Result<(), StateError> {
        let secrets = self.secret_marker_directory(id)?;
        secrets.replace(&format!("{id}.marker"), marker)?;
        match secrets.remove(&format!("{id}.candidate")) {
            Ok(()) => secrets.sync(),
            Err(StateError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    fn secret_marker_directory(&self, id: &str) -> Result<Directory, StateError> {
        if !valid_hash(id) {
            return Err(StateError::invalid(
                &self.store.root.path,
                "无效的验证标记 ID",
            ));
        }
        self.store.root.private_child("secrets")
    }

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

    /// Save a planner-produced review and return its current executable ID.
    pub fn save_plan(
        &self,
        logical_host: &str,
        plan: &Plan,
        resources: &ResourceSet,
        supplied: &BTreeMap<ContentDigest, Vec<u8>>,
    ) -> Result<Plan, StateError> {
        let name = self.plan_name(&plan.id)?;
        let path = self.store.plans.path.join(&name);
        checked_fingerprint(&path, &plan.id, logical_host, plan, resources, None)?;

        let now = (self.store.clock)();
        let history = self.plan_history()?;
        let prior = history
            .iter()
            .filter(|review| {
                review.fingerprint == plan.id
                    && !review.retired
                    && !self.retention.is_expired(review.record.created_at, now)
            })
            .max_by_key(|review| (review.record.created_at, review.record.issuance));
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
                pending.push((key, bytes.as_slice()));
            }
        }

        if let Some(prior) = prior {
            self.publish_objects(pending)?;
            self.store.plans.sync()?;
            return Ok(prior.record.plan.clone());
        }

        let (issued, issuance) = self.issue_plan(plan, &history, now)?;
        let name = self.plan_name(&issued.id)?;
        let record = Record {
            format_version: FORMAT_VERSION,
            logical_host,
            created_at: now,
            issuance,
            plan: &issued,
            resources,
        };
        let data = rmp_serde::to_vec_named(&record).map_err(|err| {
            StateError::invalid(
                &self.store.plans.path.join(&name),
                format!("无法编码计划：{err}"),
            )
        })?;
        let collection = self.review_collection(&history, referenced(plan, resources), now)?;
        self.publish_objects(pending)?;
        self.collect_reviews(collection)?;
        self.store.plans.publish(&name, &data)?;
        Ok(issued)
    }

    fn publish_objects(&self, pending: Vec<(&str, &[u8])>) -> Result<(), StateError> {
        for (key, bytes) in pending {
            self.store.objects.publish(key, bytes)?;
        }
        self.store.objects.sync()
    }

    pub fn load_plan(
        &self,
        id: &str,
        binding: PlanBinding<'_>,
    ) -> Result<ReviewedPlan, StateError> {
        self.load_review(id, binding, false)
    }

    /// Resume an unfinished, bound run without enforcing the new-execution deadline.
    pub fn load_run_plan(
        &self,
        run_id: &str,
        binding: PlanBinding<'_>,
    ) -> Result<ReviewedPlan, StateError> {
        let run = self.load_run(run_id, binding)?;
        if run.finished {
            return Err(StateError::invalid(
                &self.store.root.path,
                format!("执行 {run_id} 已完成"),
            ));
        }
        self.load_review(&run.plan_id, binding, true)
    }

    fn load_review(
        &self,
        id: &str,
        binding: PlanBinding<'_>,
        resume: bool,
    ) -> Result<ReviewedPlan, StateError> {
        let name = self.plan_name(id)?;
        let path = self.store.plans.path.join(&name);
        let retired = self.expired_record(&name)?;
        if let Some(retired) = &retired {
            check_binding(
                &path,
                &retired.repository_id,
                &retired.logical_host,
                &retired.target,
                binding,
            )?;
            if !resume {
                return Err(StateError::ExpiredPlan {
                    id: id.into(),
                    expired_at: retired.expired_at,
                    mode: retired.mode,
                });
            }
        }
        let Some(record) = self.load_record(&name)? else {
            if let Some(retired) = retired {
                return Err(StateError::ExpiredPlan {
                    id: id.into(),
                    expired_at: retired.expired_at,
                    mode: retired.mode,
                });
            }
            return Err(StateError::MissingPlan {
                id: id.into(),
                path,
            });
        };
        check_binding(
            &path,
            &record.plan.repository_id,
            &record.logical_host,
            &record.plan.target,
            binding,
        )?;
        checked_fingerprint(
            &path,
            id,
            &record.logical_host,
            &record.plan,
            &record.resources,
            record.issuance,
        )?;
        if !resume
            && self
                .retention
                .is_expired(record.created_at, (self.store.clock)())
        {
            return Err(StateError::ExpiredPlan {
                id: id.into(),
                expired_at: self.retention.expires_at(record.created_at),
                mode: record.plan.mode(),
            });
        }
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
}

fn checked_fingerprint(
    path: &Path,
    id: &str,
    logical_host: &str,
    plan: &Plan,
    resources: &ResourceSet,
    issuance: Option<u64>,
) -> Result<String, StateError> {
    let encode = |err| StateError::invalid(path, format!("无法计算计划摘要：{err}"));
    let fingerprint = plan_id(plan, Some(logical_host)).map_err(encode)?;
    let issued = issuance.map(|generation| retention::renewed_id(&fingerprint, generation));
    if plan.id != id
        || issued.as_deref().unwrap_or(&fingerprint) != id
        || resource_digest(resources).map_err(encode)? != plan.resource_digest
    {
        return Err(StateError::invalid(
            path,
            "计划 ID 或资源摘要与审阅内容不符",
        ));
    }
    Ok(fingerprint)
}

fn check_binding(
    path: &Path,
    repository: &str,
    host: &str,
    target: &TargetIdentity,
    binding: PlanBinding<'_>,
) -> Result<(), StateError> {
    if repository != binding.repository_id {
        return Err(StateError::invalid(
            path,
            format!(
                "计划仓库 {repository} 与当前仓库 {} 不符",
                binding.repository_id
            ),
        ));
    }
    if host != binding.logical_host || target != binding.target {
        return Err(StateError::invalid(
            path,
            format!(
                "计划主机 {host}（{}）与当前主机 {}（{}）不符",
                target.id, binding.logical_host, binding.target.id
            ),
        ));
    }
    Ok(())
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

#[cfg(test)]
mod retention_tests;
