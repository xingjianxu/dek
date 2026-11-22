use std::{
    collections::{BTreeMap, BTreeSet},
    io::Cursor,
    num::NonZeroU64,
};

use dek_core::{ContentDigest, Plan, PlanMode, TargetIdentity};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    OwnedRecord, PlanBinding, StateError, StateGuard, check_binding, checked_fingerprint,
    fs::Directory, referenced, valid_hash,
};

const DEFAULT_TTL_SECS: u64 = 7 * 24 * 60 * 60;
const MARKER_VERSION: u32 = 1;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct RetentionPolicy {
    plan_ttl_seconds: NonZeroU64,
}

impl RetentionPolicy {
    pub(super) fn read(root: &Directory) -> Result<Self, StateError> {
        let Some(data) = root.read("retention.json")? else {
            return Ok(Self {
                plan_ttl_seconds: NonZeroU64::new(DEFAULT_TTL_SECS).unwrap(),
            });
        };
        serde_json::from_slice(&data).map_err(|err| {
            StateError::invalid(
                &root.path.join("retention.json"),
                format!("无法读取计划保留策略：{err}"),
            )
        })
    }

    pub(super) fn is_expired(&self, created_at: u64, now: u64) -> bool {
        now.saturating_sub(created_at) >= self.plan_ttl_seconds.get()
    }

    pub(super) fn expires_at(&self, created_at: u64) -> u64 {
        created_at.saturating_add(self.plan_ttl_seconds.get())
    }
}

pub(super) struct CheckedReview {
    pub(super) fingerprint: String,
    pub(super) record: OwnedRecord,
    pub(super) retired: bool,
}

/// No desired resources or content references survive in this marker.
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ExpiredReview {
    version: u32,
    id: String,
    pub(super) expired_at: u64,
    pub(super) repository_id: String,
    pub(super) logical_host: String,
    pub(super) target: TargetIdentity,
    #[serde(default)]
    pub(super) mode: PlanMode,
}

pub(super) struct ReviewCollection {
    expired: Vec<Retirement>,
    unreferenced: Vec<String>,
}

struct Retirement {
    name: String,
    new_marker: Option<Vec<u8>>,
    remove_plan: bool,
}

impl StateGuard<'_> {
    pub(super) fn plan_history(&self) -> Result<Vec<CheckedReview>, StateError> {
        let mut reviews = Vec::new();
        for name in self.store.plans.file_names()? {
            let Some(id) = name.strip_suffix(".mpk") else {
                continue;
            };
            let path = self.store.plans.path.join(&name);
            if !valid_hash(id) {
                return Err(StateError::invalid(&path, "计划文件名无效"));
            }
            if let Some(record) = self.load_record(&name)? {
                let fingerprint = checked_fingerprint(
                    &path,
                    id,
                    &record.logical_host,
                    &record.plan,
                    &record.resources,
                    record.issuance,
                )?;
                let marker = self.expired_record(&name)?;
                if let Some(marker) = &marker {
                    check_binding(
                        &path,
                        &marker.repository_id,
                        &marker.logical_host,
                        &marker.target,
                        PlanBinding {
                            repository_id: &record.plan.repository_id,
                            logical_host: &record.logical_host,
                            target: &record.plan.target,
                        },
                    )?;
                }
                reviews.push(CheckedReview {
                    fingerprint,
                    record,
                    retired: marker.is_some(),
                });
            }
        }
        Ok(reviews)
    }

    pub(super) fn issue_plan(
        &self,
        blueprint: &Plan,
        history: &[CheckedReview],
        now: u64,
    ) -> Result<(Plan, Option<u64>), StateError> {
        let used = |id: &str| -> Result<bool, StateError> {
            Ok(history.iter().any(|review| review.record.plan.id == id)
                || self.expired_record(&self.plan_name(id)?)?.is_some())
        };
        if !used(&blueprint.id)? {
            return Ok((blueprint.clone(), None));
        }

        let mut generation = now;
        loop {
            let id = renewed_id(&blueprint.id, generation);
            if !used(&id)? {
                return Ok((
                    Plan {
                        id,
                        ..blueprint.clone()
                    },
                    Some(generation),
                ));
            }
            generation = generation.checked_add(1).ok_or_else(|| {
                StateError::invalid(&self.store.plans.path, "计划续发序号已达上限")
            })?;
        }
    }

    pub(super) fn review_collection(
        &self,
        history: &[CheckedReview],
        candidate: BTreeSet<&ContentDigest>,
        now: u64,
    ) -> Result<ReviewCollection, StateError> {
        let indexed: BTreeMap<_, _> = history
            .iter()
            .map(|review| (review.record.plan.id.as_str(), &review.record))
            .collect();
        let mut pinned = BTreeSet::new();
        for run in self.active_runs()? {
            let record = indexed.get(run.plan_id.as_str()).ok_or_else(|| {
                StateError::invalid(
                    &self.store.plans.path,
                    format!("待恢复执行 {} 缺少计划 {}", run.id, run.plan_id),
                )
            })?;
            if record.plan.repository_id != run.repository_id
                || record.logical_host != run.logical_host
                || record.plan.target != run.target
            {
                return Err(StateError::invalid(
                    &self.store.plans.path,
                    format!("待恢复执行 {} 的计划归属不符", run.id),
                ));
            }
            pinned.insert(run.plan_id);
        }

        let mut keep = BTreeSet::new();
        let mut expired = Vec::new();
        for digest in candidate {
            keep.insert(self.object_key(digest)?.to_owned());
        }
        for review in history {
            let record = &review.record;
            let is_pinned = pinned.contains(&record.plan.id);
            let is_expired = review.retired || self.retention.is_expired(record.created_at, now);
            if is_pinned || !is_expired {
                for digest in referenced(&record.plan, &record.resources) {
                    keep.insert(self.object_key(digest)?.to_owned());
                }
            }
            if is_expired && (!is_pinned || !review.retired) {
                expired.push(self.prepare_retirement(review, !is_pinned)?);
            }
        }

        let unreferenced = self
            .store
            .objects
            .file_names()?
            .into_iter()
            .filter(|name| valid_hash(name) && !keep.contains(name))
            .collect();
        Ok(ReviewCollection {
            expired,
            unreferenced,
        })
    }

    fn prepare_retirement(
        &self,
        review: &CheckedReview,
        remove_plan: bool,
    ) -> Result<Retirement, StateError> {
        let record = &review.record;
        let name = self.plan_name(&record.plan.id)?;
        let new_marker =
            if review.retired {
                None
            } else {
                let marker = ExpiredReview {
                    version: MARKER_VERSION,
                    id: record.plan.id.clone(),
                    expired_at: self.retention.expires_at(record.created_at),
                    repository_id: record.plan.repository_id.clone(),
                    logical_host: record.logical_host.clone(),
                    target: record.plan.target.clone(),
                    mode: record.plan.mode(),
                };
                let path = self.store.expired.path.join(&name);
                Some(rmp_serde::to_vec_named(&marker).map_err(|err| {
                    StateError::invalid(&path, format!("无法编码过期标记：{err}"))
                })?)
            };
        Ok(Retirement {
            name,
            new_marker,
            remove_plan,
        })
    }

    pub(super) fn collect_reviews(&self, collection: ReviewCollection) -> Result<(), StateError> {
        for retirement in &collection.expired {
            if let Some(bytes) = &retirement.new_marker {
                self.store.expired.publish(&retirement.name, bytes)?;
            }
        }
        self.store.expired.sync()?;
        for retirement in collection.expired {
            if retirement.remove_plan {
                self.store.plans.remove(&retirement.name)?;
            }
        }
        self.store.plans.sync()?;
        for name in collection.unreferenced {
            self.store.objects.remove(&name)?;
        }
        self.store.objects.sync()
    }

    pub(super) fn expired_record(&self, name: &str) -> Result<Option<ExpiredReview>, StateError> {
        let Some(bytes) = self.store.expired.read(name)? else {
            return Ok(None);
        };
        let path = self.store.expired.path.join(name);
        let mut cursor = Cursor::new(&bytes);
        let record: ExpiredReview = rmp_serde::from_read(&mut cursor)
            .map_err(|err| StateError::invalid(&path, format!("无法解码过期标记：{err}")))?;
        if cursor.position() != bytes.len() as u64
            || record.version != MARKER_VERSION
            || self.plan_name(&record.id)? != name
        {
            return Err(StateError::invalid(&path, "过期标记的版本或标识无效"));
        }
        Ok(Some(record))
    }
}

pub(super) fn renewed_id(fingerprint: &str, generation: u64) -> String {
    let mut hash = Sha256::new();
    hash.update(b"dek-plan-issuance-v1\0");
    hash.update(fingerprint);
    hash.update(generation.to_be_bytes());
    format!("{:x}", hash.finalize())
}
