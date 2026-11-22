//! Transfer only the static objects requested by the authenticated target.

use std::collections::{BTreeMap, BTreeSet};

use dek_core::{
    ContentDigest,
    target_protocol::{CONTENT_CHUNK_BYTES, PlanRequest, RemotePlan, TargetRequest, TargetResult},
};

use super::{SshError, SshErrorKind, SshSession, invalid, ssh_error};

const PLAN: &str = "远程规划";

impl SshSession {
    pub fn request_plan(
        &mut self,
        request: PlanRequest,
        contents: &BTreeMap<ContentDigest, Vec<u8>>,
    ) -> Result<RemotePlan, SshError> {
        let repository_id = request.repository_id.clone();
        let referenced: BTreeSet<_> = request
            .resources
            .content_digests()
            .into_iter()
            .cloned()
            .collect();
        let first = self.plan_exchange(TargetRequest::PreparePlan {
            request: Box::new(request),
        })?;
        let TargetResult::MissingContents { mut digests } = first else {
            return Err(self.plan_error(invalid("expected missing content digests")));
        };
        // Other planners may collect unreferenced objects between exchanges.
        // The target rechecks at completion and can request those objects again.
        for _ in 0..3 {
            self.upload_contents(digests, &referenced, contents)?;
            match self.plan_exchange(TargetRequest::FinishPlan)? {
                TargetResult::Planned(review)
                    if review.plan.repository_id == repository_id
                        && review.plan.target == self.capabilities.target =>
                {
                    return Ok(*review);
                }
                TargetResult::Planned(_) => {
                    return Err(
                        self.plan_error(invalid("plan repository or target binding did not match"))
                    );
                }
                TargetResult::MissingContents { digests: missing } => digests = missing,
                _ => return Err(self.plan_error(invalid("expected a reviewed plan"))),
            }
        }
        Err(self.plan_error(SshErrorKind::Target(
            "目标内容在协商期间反复变化，请重试规划".into(),
        )))
    }

    fn upload_contents(
        &mut self,
        digests: Vec<ContentDigest>,
        referenced: &BTreeSet<ContentDigest>,
        contents: &BTreeMap<ContentDigest, Vec<u8>>,
    ) -> Result<(), SshError> {
        let mut requested = BTreeSet::new();
        for digest in &digests {
            if !referenced.contains(digest) || !requested.insert(digest) {
                return Err(self.plan_error(invalid(
                    "target requested an unreferenced or repeated content digest",
                )));
            }
        }

        for digest in digests {
            let bytes = contents.get(&digest).ok_or_else(|| {
                self.plan_error(SshErrorKind::Target(format!(
                    "管理端缺少内容对象 {}",
                    digest.0
                )))
            })?;
            let mut chunks = bytes.chunks(CONTENT_CHUNK_BYTES).peekable();
            loop {
                let chunk = chunks.next().unwrap_or_default();
                let complete = chunks.peek().is_none();
                let reply = self.plan_exchange(TargetRequest::UploadContent {
                    digest: digest.clone(),
                    chunk: chunk.to_vec(),
                    complete,
                })?;
                if reply != TargetResult::ContentAccepted {
                    return Err(self.plan_error(invalid("expected a content acknowledgement")));
                }
                if complete {
                    break;
                }
            }
        }
        Ok(())
    }

    fn plan_exchange(&mut self, operation: TargetRequest) -> Result<TargetResult, SshError> {
        match self.request(operation)? {
            TargetResult::Error { message } => Err(self.plan_error(SshErrorKind::Target(message))),
            result => Ok(result),
        }
    }

    fn plan_error(&self, kind: SshErrorKind) -> SshError {
        ssh_error(&self.alias, PLAN, kind)
    }
}
