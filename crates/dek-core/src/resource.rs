use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::Origin;

/// Content addressed by the digest supplied by the producing component.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ContentDigest(pub String);

/// The complete set of managed resources for the selected host.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceSet {
    pub resources: Vec<Resource>,
}

impl ResourceSet {
    /// Digests of all content objects the target may need for this set.
    pub fn content_digests(&self) -> BTreeSet<&ContentDigest> {
        let mut digests = BTreeSet::new();
        for resource in &self.resources {
            match &resource.desired {
                DesiredResource::File(FileResource {
                    state: FileState::Present(content),
                    ..
                }) => {
                    digests.insert(&content.digest);
                }
                DesiredResource::SystemdUnit(unit) => unit.definition.add_digests(&mut digests),
                DesiredResource::AuthorizedKeys(keys) => digests.extend(&keys.key_files),
                DesiredResource::AgeSecret(secret) => {
                    digests.insert(&secret.ciphertext);
                }
                _ => {}
            }
        }
        digests
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Resource {
    pub desired: DesiredResource,
    #[serde(default)]
    pub origins: Vec<ResourceOrigin>,
    #[serde(default)]
    pub depends_on: Vec<ResourceId>,
    #[serde(default)]
    pub notifications: Vec<ServiceNotification>,
}

impl Resource {
    pub fn id(&self) -> ResourceId {
        self.desired.id()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceOrigin {
    pub module: String,
    pub origin: Origin,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ResourceId {
    Package(PackageId),
    User(String),
    Group(String),
    /// The original target path spelling, shared by files, directories, and age outputs.
    Path(String),
    SystemdUnit(UnitId),
    AuthorizedKeys(String),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ResourceKind {
    Package,
    User,
    Group,
    Directory,
    File,
    SystemdUnit,
    AuthorizedKeys,
    AgeSecret,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DesiredResource {
    Package(PackageResource),
    User(UserResource),
    Group(GroupResource),
    Directory(DirectoryResource),
    File(FileResource),
    SystemdUnit(SystemdUnitResource),
    AuthorizedKeys(AuthorizedKeysResource),
    AgeSecret(AgeSecretResource),
}

impl DesiredResource {
    pub fn id(&self) -> ResourceId {
        match self {
            Self::Package(resource) => ResourceId::Package(resource.package.clone()),
            Self::User(resource) => ResourceId::User(resource.name.clone()),
            Self::Group(resource) => ResourceId::Group(resource.name.clone()),
            Self::Directory(resource) => ResourceId::Path(resource.path.clone()),
            Self::File(resource) => ResourceId::Path(resource.path.clone()),
            Self::SystemdUnit(resource) => ResourceId::SystemdUnit(resource.unit.clone()),
            Self::AuthorizedKeys(resource) => ResourceId::AuthorizedKeys(resource.user.clone()),
            Self::AgeSecret(resource) => ResourceId::Path(resource.path.clone()),
        }
    }

    pub fn kind(&self) -> ResourceKind {
        match self {
            Self::Package(_) => ResourceKind::Package,
            Self::User(_) => ResourceKind::User,
            Self::Group(_) => ResourceKind::Group,
            Self::Directory(_) => ResourceKind::Directory,
            Self::File(_) => ResourceKind::File,
            Self::SystemdUnit(_) => ResourceKind::SystemdUnit,
            Self::AuthorizedKeys(_) => ResourceKind::AuthorizedKeys,
            Self::AgeSecret(_) => ResourceKind::AgeSecret,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PackageId {
    pub source: PackageSource,
    pub name: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PackageSource {
    Official,
    Aur,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PackageResource {
    pub package: PackageId,
    pub state: Presence,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Presence {
    Present,
    Absent,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UserResource {
    pub name: String,
    pub kind: Option<UserKind>,
    pub uid: Option<u32>,
    pub group: Option<String>,
    pub extra_groups: Option<Vec<String>>,
    pub home: Option<String>,
    pub shell: Option<String>,
    pub description: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum UserKind {
    Normal,
    System,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GroupResource {
    pub name: String,
    pub gid: Option<u32>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FileAttributes {
    pub owner: Option<String>,
    pub group: Option<String>,
    pub mode: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DirectoryResource {
    pub path: String,
    #[serde(default)]
    pub attributes: FileAttributes,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FileResource {
    pub path: String,
    pub state: FileState,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum FileState {
    Present(FileContent),
    Absent,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FileContent {
    pub digest: ContentDigest,
    #[serde(default)]
    pub attributes: FileAttributes,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UnitId {
    pub scope: UnitScope,
    /// The full name including the systemd suffix, such as `backup.timer`.
    pub name: String,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum UnitScope {
    System,
    UserGlobal,
    User { user: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SystemdUnitResource {
    pub unit: UnitId,
    #[serde(default)]
    pub definition: UnitDefinition,
    pub enabled: Option<bool>,
    pub running: Option<bool>,
    pub activates: Option<UnitId>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UnitDefinition {
    pub content: Option<ContentDigest>,
    #[serde(default)]
    pub drop_ins: BTreeMap<String, ContentDigest>,
}

impl UnitDefinition {
    pub(crate) fn add_digests<'a>(&'a self, digests: &mut BTreeSet<&'a ContentDigest>) {
        digests.extend(&self.content);
        digests.extend(self.drop_ins.values());
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthorizedKeysResource {
    pub user: String,
    #[serde(default)]
    pub keys: Vec<String>,
    #[serde(default)]
    pub key_files: Vec<ContentDigest>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgeSecretResource {
    pub path: String,
    pub ciphertext: ContentDigest,
    pub identity_paths: Vec<String>,
    #[serde(default)]
    pub attributes: FileAttributes,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceNotification {
    pub unit: UnitId,
    pub effect: ServiceEffect,
    pub when: NotifyWhen,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ServiceEffect {
    Reload,
    Restart,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum NotifyWhen {
    Changed,
    ContentChanged,
}
