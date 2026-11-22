//! Shared execution status and pending notification records.

use serde::{Deserialize, Serialize};

use crate::{ServiceEffect, UnitId};

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
