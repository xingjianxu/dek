use std::{collections::BTreeSet, error::Error, fmt};

use serde::{Deserialize, Serialize};

use crate::{
    AgeSecretResource, AuthorizedKeysResource, ContentDigest, DirectoryResource, FileAttributes,
    FileContent, GroupResource, InputObservation, NotifyWhen, Observation, PackageId, ResourceId,
    ResourceKind, ResourceOrigin, ServiceEffect, UnitDefinition, UnitId, UnitScope, UserResource,
};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TargetIdentity {
    pub id: String,
    pub os: String,
    pub distribution: Option<String>,
    pub architecture: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TargetCapabilities {
    pub target: TargetIdentity,
    pub protocol_version: u32,
    pub resources: Vec<ResourceCapability>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceCapability {
    pub kind: ResourceKind,
    /// Version of the resource interface implemented by the target backend.
    pub version: u32,
    pub attributes: BTreeSet<String>,
}

/// The exact actions and inputs that were reviewed for one repository and target.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Plan {
    pub format_version: PlanVersion,
    pub id: String,
    pub repository_id: String,
    pub target: TargetIdentity,
    pub resource_digest: ContentDigest,
    pub used_capabilities: Vec<ResourceCapability>,
    pub used_inputs: Vec<InputObservation>,
    pub actions: Vec<Action>,
}

impl Plan {
    /// Digests of the content objects required to perform the reviewed actions.
    pub fn content_digests(&self) -> BTreeSet<&ContentDigest> {
        let mut digests = BTreeSet::new();
        for action in &self.actions {
            match &action.kind {
                ActionKind::WriteFile { content, .. } => {
                    digests.insert(&content.digest);
                }
                ActionKind::DeployUnit { definition, .. } => definition.add_digests(&mut digests),
                ActionKind::UpdateAuthorizedKeys { keys, .. } => digests.extend(&keys.key_files),
                ActionKind::DeployAgeSecret(secret)
                | ActionKind::SetAgeSecretAttributes(secret) => {
                    digests.insert(&secret.ciphertext);
                }
                _ => {}
            }
        }
        digests
    }
}

/// The plan format version on the wire. Deserialization accepts supported versions only.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "u32", into = "u32")]
pub enum PlanVersion {
    V1,
}

impl TryFrom<u32> for PlanVersion {
    type Error = UnsupportedPlanVersion;

    fn try_from(version: u32) -> Result<Self, Self::Error> {
        match version {
            1 => Ok(Self::V1),
            version => Err(UnsupportedPlanVersion(version)),
        }
    }
}

impl From<PlanVersion> for u32 {
    fn from(version: PlanVersion) -> Self {
        match version {
            PlanVersion::V1 => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnsupportedPlanVersion(pub u32);

impl fmt::Display for UnsupportedPlanVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unsupported plan format version {}", self.0)
    }
}

impl Error for UnsupportedPlanVersion {}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Action {
    pub id: String,
    pub kind: ActionKind,
    pub reason: String,
    pub origins: Vec<ResourceOrigin>,
    pub depends_on: Vec<String>,
    pub preconditions: Vec<Precondition>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum ActionKind {
    WriteFile {
        path: String,
        content: FileContent,
    },
    SetFileAttributes {
        path: String,
        attributes: FileAttributes,
    },
    RemoveFile {
        path: String,
    },
    EnsureDirectory(DirectoryResource),
    EnsureUser(UserResource),
    SetUserPrimaryGroup {
        user: String,
        group: String,
    },
    EnsureGroup(GroupResource),
    InstallPackage {
        package: PackageId,
        candidate_version: String,
    },
    RemovePackage {
        package: PackageId,
    },
    DeployUnit {
        unit: UnitId,
        definition: UnitDefinition,
    },
    ReloadManager(UnitScope),
    SetUnitEnabled {
        unit: UnitId,
        enabled: bool,
    },
    SetUnitRunning {
        unit: UnitId,
        running: bool,
    },
    NotifyService {
        unit: UnitId,
        effect: ServiceEffect,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        sources: Vec<NotificationSource>,
    },
    UpdateAuthorizedKeys {
        keys: AuthorizedKeysResource,
        home: String,
        managed_digest: ContentDigest,
    },
    DeployAgeSecret(AgeSecretResource),
    SetAgeSecretAttributes(AgeSecretResource),
}

/// A source that can request a specific effect once its actual change is known.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NotificationSource {
    pub action: String,
    pub effect: ServiceEffect,
    pub when: NotifyWhen,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum Precondition {
    File {
        path: String,
        expected: FileObservation,
    },
    Directory {
        path: String,
        expected: Option<DirectoryObservation>,
    },
    AgeSecret {
        path: String,
        expected: AgeSecretObservation,
    },
    UserAccount {
        user: String,
        expected: Option<UserObservation>,
    },
    GroupAccount {
        group: String,
        expected: Option<u32>,
    },
    PackageVersion {
        package: PackageId,
        expected: Option<String>,
    },
    UnitEnabled {
        unit: UnitId,
        expected: bool,
    },
    UnitRunning {
        unit: UnitId,
        expected: bool,
    },
    ResourceState {
        resource: ResourceId,
        expected: Observation,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FileObservation {
    Missing,
    Present(FileContent),
}

/// Target directory identity and the metadata present in the desired declaration.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DirectoryObservation {
    pub device: u64,
    pub inode: u64,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub mode: Option<u32>,
}

/// Local account identity and observed managed fields, with no credential data.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UserObservation {
    pub uid: u32,
    pub primary_gid: Option<u32>,
    pub extra_groups: Option<BTreeSet<String>>,
    pub home: Option<String>,
    pub shell: Option<String>,
    pub description: Option<String>,
}

/// A target-side verification result. It deliberately has no plaintext fingerprint.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum AgeSecretObservation {
    Missing,
    Present {
        verification: AgeSecretVerification,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        revision: Option<AgeSecretRevision>,
        uid: Option<u32>,
        gid: Option<u32>,
        mode: u32,
    },
}

/// Filesystem identity and change time for reviewed target bytes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgeSecretRevision {
    pub device: u64,
    pub inode: u64,
    pub changed_seconds: i64,
    pub changed_nanoseconds: i64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AgeSecretVerification {
    Matched,
    Drifted,
    Unverified,
    InputsChanged,
}
