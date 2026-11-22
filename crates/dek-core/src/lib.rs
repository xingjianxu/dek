//! 共享领域模型、协议与来源诊断。

mod config;
mod origin;
mod plan;
pub mod protocol;
mod resource;
pub mod target_protocol;

pub use config::{
    EffectiveOption, HostConfig, InputKind, InputObservation, InvalidOptionPath, Observation,
    OptionAssignment, OptionDefinition, OptionPath, Priority, ProfileRef, UnitSpec,
};
pub use origin::{InvalidJsonPointer, JsonPointer, Origin};

pub use plan::{
    Action, ActionKind, AgeSecretObservation, AgeSecretRevision, AgeSecretVerification,
    DirectoryObservation, FileObservation, Plan, PlanVersion, Precondition, ResourceCapability,
    TargetCapabilities, TargetIdentity, UnsupportedPlanVersion, UserObservation,
};
pub use resource::{
    AgeSecretResource, AuthorizedKeysResource, ContentDigest, DesiredResource, DirectoryResource,
    FileAttributes, FileContent, FileResource, FileState, GroupResource, NotifyWhen, PackageId,
    PackageResource, PackageSource, Presence, Resource, ResourceId, ResourceKind, ResourceOrigin,
    ResourceSet, ServiceEffect, ServiceNotification, SystemdUnitResource, UnitDefinition, UnitId,
    UnitScope, UserKind, UserResource,
};
