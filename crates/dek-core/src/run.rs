//! Shared execution status and pending notification records.

use serde::{Deserialize, Serialize};

use crate::{Action, ServiceEffect, TargetIdentity, UnitId};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum ActionResult {
    Pending,
    Unverified,
    Deferred {
        message: String,
    },
    Verified {
        changed: bool,
    },
    Failed {
        message: String,
        #[serde(
            default,
            rename = "notificationJobFailed",
            skip_serializing_if = "std::ops::Not::not"
        )]
        notification_job_failed: bool,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunNotification {
    pub action_id: String,
    pub unit: UnitId,
    pub effect: ServiceEffect,
    pub depends_on: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PendingRunNotification {
    pub run_id: String,
    pub notification: RunNotification,
    pub result: ActionResult,
    pub prerequisites_verified: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunAction {
    pub action: Action,
    pub attempt: u32,
    pub result: ActionResult,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_notification_effect: Option<ServiceEffect>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_diagnostic: Option<String>,
}

/// Public execution status, independent of the target record's storage format.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunStatus {
    pub id: String,
    pub plan_id: String,
    pub logical_host: String,
    pub target: TargetIdentity,
    pub request_id: String,
    pub created_at: u64,
    pub actions: Vec<RunAction>,
    pub pending_notifications: Vec<RunNotification>,
    pub finished: bool,
}
