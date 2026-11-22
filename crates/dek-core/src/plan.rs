use std::{collections::BTreeSet, error::Error, fmt};

use serde::{Deserialize, Serialize};

use crate::{
    AgeSecretResource, AuthorizedKeysResource, ContentDigest, DirectoryResource, FileAttributes,
    FileContent, GroupResource, InputObservation, Observation, PackageId, ResourceId, ResourceKind,
    ResourceOrigin, ServiceEffect, UnitDefinition, UnitId, UnitScope, UserResource,
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
                ActionKind::UpdateAuthorizedKeys(keys) => digests.extend(&keys.key_files),
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
    },
    UpdateAuthorizedKeys(AuthorizedKeysResource),
    DeployAgeSecret(AgeSecretResource),
    SetAgeSecretAttributes(AgeSecretResource),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum Precondition {
    File {
        path: String,
        expected: FileObservation,
    },
    AgeSecret {
        path: String,
        expected: AgeSecretObservation,
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

/// A target-side verification result. It deliberately has no plaintext fingerprint.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum AgeSecretObservation {
    Missing,
    Present {
        verification: AgeSecretVerification,
        uid: Option<u32>,
        gid: Option<u32>,
        mode: u32,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AgeSecretVerification {
    Matched,
    Drifted,
    Unverified,
    InputsChanged,
}
