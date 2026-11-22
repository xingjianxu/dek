//! Negotiate and publish verified content before observing the target.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    path::Path,
};

use dek_core::{
    ContentDigest,
    target_protocol::{PlanRequest, RemotePlan, TargetRequest, TargetResult},
};

use crate::{backend::BackendRegistry, planner::plan_for_host_with_contents};

use super::{PlanBinding, StateStore};

/// One authenticated connection's in-progress plan and object upload.
pub struct PlanSession<'a> {
    root: &'a Path,
    pending: Option<PlanTransfer>,
}

struct PlanTransfer {
    store: StateStore,
    request: PlanRequest,
    missing: BTreeSet<ContentDigest>,
    upload: Option<(ContentDigest, Vec<u8>)>,
}

impl<'a> PlanSession<'a> {
    pub fn new(root: &'a Path) -> Self {
        Self {
            root,
            pending: None,
        }
    }

    pub fn dispatch(
        &mut self,
        registry: &BackendRegistry<'_>,
        operation: TargetRequest,
    ) -> Result<TargetResult, TargetRequest> {
        let result = match operation {
            TargetRequest::PreparePlan { request } => self.prepare(*request),
            TargetRequest::UploadContent {
                digest,
                chunk,
                complete,
            } => self.upload(digest, chunk, complete),
            TargetRequest::FinishPlan => self.finish(registry),
            other => return Err(other),
        };
        Ok(result.unwrap_or_else(|error| {
            self.pending = None;
            TargetResult::Error {
                message: error.to_string(),
            }
        }))
    }

    fn prepare(&mut self, request: PlanRequest) -> Result<TargetResult, Box<dyn Error>> {
        if request.repository_id.trim().is_empty() || request.logical_host.trim().is_empty() {
            return Err("远程计划需要仓库标识和清单主机名".into());
        }
        let store = StateStore::open(self.root)
            .map_err(|error| format!("{error}。请在目标机配置 SSH 账户持有的私有状态目录"))?;
        let mut missing = BTreeSet::new();
        {
            let guard = store.lock()?;
            for digest in request.resources.content_digests() {
                if !guard.has_object(digest)? {
                    missing.insert(digest.clone());
                }
            }
        }
        let digests = missing.iter().cloned().collect();
        self.pending = Some(PlanTransfer {
            store,
            request,
            missing,
            upload: None,
        });
        Ok(TargetResult::MissingContents { digests })
    }

    fn upload(
        &mut self,
        digest: ContentDigest,
        chunk: Vec<u8>,
        complete: bool,
    ) -> Result<TargetResult, Box<dyn Error>> {
        let transfer = self.pending.as_mut().ok_or("尚未提交远程规划声明")?;
        if !transfer.missing.contains(&digest) {
            return Err(format!("目标未请求内容对象 {}", digest.0).into());
        }
        if chunk.is_empty() && !complete {
            return Err(format!("内容对象 {} 的非结尾分片为空", digest.0).into());
        }
        let (current, bytes) = transfer
            .upload
            .get_or_insert_with(|| (digest.clone(), Vec::new()));
        if *current != digest {
            return Err(format!("内容对象 {} 尚未传输完毕", current.0).into());
        }
        bytes.extend(chunk);
        if complete {
            transfer.store.lock()?.put_object(&digest, bytes)?;
            transfer.upload = None;
            transfer.missing.remove(&digest);
        }
        Ok(TargetResult::ContentAccepted)
    }

    fn finish(&mut self, registry: &BackendRegistry<'_>) -> Result<TargetResult, Box<dyn Error>> {
        let transfer = self.pending.as_mut().ok_or("尚未提交远程规划声明")?;
        if let Some((digest, _)) = &transfer.upload {
            return Err(format!("内容对象 {} 尚未传输完毕", digest.0).into());
        }
        let request = &transfer.request;
        // Recheck under the state lock. Retention in other connections can
        // collect unreferenced objects between individual requests.
        let guard = transfer.store.lock()?;
        let mut contents = BTreeMap::new();
        transfer.missing.clear();
        for digest in request.resources.content_digests() {
            if let Some(bytes) = guard.object(digest)? {
                contents.insert(digest.clone(), bytes);
            } else {
                transfer.missing.insert(digest.clone());
            }
        }
        if !transfer.missing.is_empty() {
            return Ok(TargetResult::MissingContents {
                digests: transfer.missing.iter().cloned().collect(),
            });
        }
        let plan = plan_for_host_with_contents(
            registry,
            &request.repository_id,
            &request.logical_host,
            &request.resources,
            &request.used_inputs,
            &contents,
        )?;
        let pending_notifications = guard.pending_notifications(PlanBinding {
            repository_id: &request.repository_id,
            logical_host: &request.logical_host,
            target: registry.target(),
        })?;
        let plan = guard.save_plan(&request.logical_host, &plan, &request.resources, &contents)?;
        drop(guard);
        self.pending = None;
        Ok(TargetResult::Planned(Box::new(RemotePlan {
            plan,
            pending_notifications,
        })))
    }
}
