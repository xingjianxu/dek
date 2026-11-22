use std::{
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
    fs,
    path::{Component, Path},
};

use dek_core::{
    AgeSecretResource, AuthorizedKeysResource, ContentDigest, DesiredResource, FileAttributes,
    FileContent, FileResource, FileState, InputKind, InputObservation, Observation, Origin,
    Resource, ResourceId, ResourceOrigin, ResourceSet, ServiceNotification,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{ExpansionError, ModuleDefinition, ModuleExpansion};

/// An expansion can retain deferred file references across evaluation runs.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModuleOutput {
    #[serde(default)]
    pub resources: Vec<DeclaredResource>,
    #[serde(default)]
    pub contents: BTreeMap<ContentDigest, Vec<u8>>,
    #[serde(default)]
    pub inputs: Vec<InputObservation>,
}

impl ModuleOutput {
    pub(super) fn validate_contents(
        &self,
        definition: &ModuleDefinition,
    ) -> Result<(), ExpansionError> {
        for (address, bytes) in &self.contents {
            if *address != digest(bytes) {
                return Err(ExpansionError(format!(
                    "expand {} at {} returned bytes that do not match {}",
                    definition.name, definition.origin, address.0
                )));
            }
        }
        Ok(())
    }

    pub(super) fn map_origins(&mut self, mut map: impl FnMut(&mut Origin)) {
        for resource in &mut self.resources {
            for origin in &mut resource.origins {
                map(origin);
            }
            match &mut resource.desired {
                ResourceDraft::File {
                    content: FileSource::Relative { origin, .. },
                    ..
                } => map(origin),
                ResourceDraft::AuthorizedKeys { key_files, .. } => {
                    for file in key_files {
                        map(&mut file.origin);
                    }
                }
                ResourceDraft::AgeSecret { ciphertext, .. } => map(&mut ciphertext.origin),
                _ => {}
            }
        }
        for input in &mut self.inputs {
            if let Some(origin) = &mut input.origin {
                map(origin);
            }
        }
    }

    pub fn add_content(&mut self, bytes: Vec<u8>) -> ContentDigest {
        let address = digest(&bytes);
        self.contents.insert(address.clone(), bytes);
        address
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeclaredResource {
    pub desired: ResourceDraft,
    #[serde(default)]
    pub origins: Vec<Origin>,
    #[serde(default)]
    pub depends_on: Vec<ResourceId>,
    #[serde(default)]
    pub notifications: Vec<ServiceNotification>,
}

impl DeclaredResource {
    pub fn new(desired: ResourceDraft) -> Self {
        Self {
            desired,
            origins: Vec::new(),
            depends_on: Vec::new(),
            notifications: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum ResourceDraft {
    Ready(DesiredResource),
    File {
        path: String,
        #[serde(default)]
        attributes: FileAttributes,
        content: FileSource,
    },
    AuthorizedKeys {
        user: String,
        #[serde(default)]
        keys: Vec<String>,
        #[serde(default)]
        key_files: Vec<RepositoryFile>,
    },
    AgeSecret {
        path: String,
        ciphertext: RepositoryFile,
        identity_paths: Vec<String>,
        #[serde(default)]
        attributes: FileAttributes,
    },
}

/// A file resolved within the configuration repository, relative to its declaration.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryFile {
    pub path: String,
    pub origin: Origin,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum FileSource {
    Inline(Vec<u8>),
    Relative { path: String, origin: Origin },
}

#[derive(Debug)]
pub struct ExpandedResources {
    pub resources: ResourceSet,
    pub contents: BTreeMap<ContentDigest, Vec<u8>>,
    pub file_inputs: Vec<InputObservation>,
}

pub fn assemble_resources(
    repo_root: &Path,
    expansions: &[ModuleExpansion],
) -> Result<ExpandedResources, ExpansionError> {
    let mut resources = BTreeMap::new();
    let mut contents = BTreeMap::new();
    let mut file_inputs = Vec::new();
    for expansion in expansions {
        expansion.output.validate_contents(&expansion.definition)?;
        contents.extend(
            expansion
                .output
                .contents
                .iter()
                .map(|(address, bytes)| (address.clone(), bytes.clone())),
        );
        for declaration in &expansion.output.resources {
            let resource = materialize(
                repo_root,
                expansion,
                declaration,
                &mut contents,
                &mut file_inputs,
            )?;
            insert_resource(&mut resources, resource)?;
        }
    }

    link_key_dependencies(&mut resources);
    check_dependencies(&resources)?;
    let resources = ResourceSet {
        resources: resources.into_values().collect(),
    };
    for resource in &resources.resources {
        for address in resource.content_digests() {
            if !contents.contains_key(address) {
                return Err(ExpansionError(format!(
                    "missing content {} for resource {:?} at {}",
                    address.0,
                    resource.id(),
                    locations(resource)
                )));
            }
        }
    }
    Ok(ExpandedResources {
        resources,
        contents,
        file_inputs,
    })
}

fn materialize(
    repo_root: &Path,
    expansion: &ModuleExpansion,
    declaration: &DeclaredResource,
    contents: &mut BTreeMap<ContentDigest, Vec<u8>>,
    inputs: &mut Vec<InputObservation>,
) -> Result<Resource, ExpansionError> {
    let mut origins = declaration.origins.clone();
    let mut files = FileMaterializer {
        repo_root,
        module: &expansion.definition.name,
        contents,
        inputs,
    };
    let desired = match &declaration.desired {
        ResourceDraft::Ready(resource) => resource.clone(),
        ResourceDraft::File {
            path,
            attributes,
            content,
        } => {
            let address = match content {
                FileSource::Inline(bytes) => files.store(bytes.clone()),
                FileSource::Relative { path, origin } => {
                    add_unique(&mut origins, [origin.clone()]);
                    files.read_relative(path, origin, false)?
                }
            };
            DesiredResource::File(FileResource {
                path: path.clone(),
                state: FileState::Present(FileContent {
                    digest: address,
                    attributes: attributes.clone(),
                }),
            })
        }
        ResourceDraft::AuthorizedKeys {
            user,
            keys,
            key_files,
        } => {
            let mut digests = Vec::new();
            for file in key_files {
                add_unique(&mut origins, [file.origin.clone()]);
                add_unique(&mut digests, [files.read_repository_file(file)?]);
            }
            DesiredResource::AuthorizedKeys(AuthorizedKeysResource {
                user: user.clone(),
                keys: keys.clone(),
                key_files: digests,
            })
        }
        ResourceDraft::AgeSecret {
            path,
            ciphertext,
            identity_paths,
            attributes,
        } => {
            add_unique(&mut origins, [ciphertext.origin.clone()]);
            DesiredResource::AgeSecret(AgeSecretResource {
                path: path.clone(),
                ciphertext: files.read_repository_file(ciphertext)?,
                identity_paths: identity_paths.clone(),
                attributes: attributes.clone(),
            })
        }
    };
    if origins.is_empty() {
        origins.push(expansion.definition.origin.clone());
    }
    let origins = origins
        .into_iter()
        .map(|origin| ResourceOrigin {
            module: expansion.definition.name.clone(),
            origin,
        })
        .collect();
    let mut depends_on = Vec::new();
    let mut notifications = Vec::new();
    add_unique(&mut depends_on, declaration.depends_on.clone());
    add_unique(&mut notifications, declaration.notifications.clone());
    Ok(Resource {
        desired,
        origins,
        depends_on,
        notifications,
    })
}

struct FileMaterializer<'a> {
    repo_root: &'a Path,
    module: &'a str,
    contents: &'a mut BTreeMap<ContentDigest, Vec<u8>>,
    inputs: &'a mut Vec<InputObservation>,
}

impl FileMaterializer<'_> {
    fn store(&mut self, bytes: Vec<u8>) -> ContentDigest {
        let address = digest(&bytes);
        self.contents.insert(address.clone(), bytes);
        address
    }

    fn read_repository_file(
        &mut self,
        source: &RepositoryFile,
    ) -> Result<ContentDigest, ExpansionError> {
        self.read_relative(&source.path, &source.origin, true)
    }

    fn read_relative(
        &mut self,
        path: &str,
        origin: &Origin,
        repository_only: bool,
    ) -> Result<ContentDigest, ExpansionError> {
        let relative = Path::new(path);
        if path.is_empty()
            || relative
                .components()
                .any(|component| matches!(component, Component::Prefix(_) | Component::RootDir))
        {
            return Err(ExpansionError(format!(
                "source file {path:?} at {origin} must be relative to the declaration"
            )));
        }
        let file = self.repo_root.join(origin.file()).with_file_name(relative);
        let module = self.module;
        let invalid = |err: std::io::Error| {
            ExpansionError(format!(
                "cannot read source file {} for {module} at {origin}: {err}",
                file.display(),
            ))
        };
        let resolved = if repository_only {
            let root = self.repo_root.canonicalize().map_err(invalid)?;
            let resolved = file.canonicalize().map_err(invalid)?;
            if !resolved.starts_with(root) {
                return Err(ExpansionError(format!(
                    "source file {} for {module} at {origin} resolves outside the repository",
                    file.display(),
                )));
            }
            resolved
        } else {
            file.clone()
        };

        let bytes = fs::metadata(&resolved)
            .and_then(|metadata| {
                if !metadata.is_file() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "source is not a regular file",
                    ));
                }
                fs::read(&resolved)
            })
            .map_err(invalid)?;
        let address = self.store(bytes);
        self.inputs.push(InputObservation {
            kind: InputKind::File,
            id: file.to_string_lossy().into_owned(),
            observation: Observation::Digest(address.0.clone()),
            origin: Some(origin.clone()),
        });
        Ok(address)
    }
}

fn link_key_dependencies(resources: &mut BTreeMap<ResourceId, Resource>) {
    let edges: Vec<_> = resources
        .values()
        .filter_map(|resource| {
            let mut depends_on = Vec::new();
            match &resource.desired {
                DesiredResource::AuthorizedKeys(keys) => {
                    let user_id = ResourceId::User(keys.user.clone());
                    if let Some(Resource {
                        desired: DesiredResource::User(user),
                        ..
                    }) = resources.get(&user_id)
                    {
                        depends_on.push(user_id);
                        if let Some(home) = &user.home {
                            add_managed_directory(
                                resources,
                                &mut depends_on,
                                format!("{}/.ssh", home.trim_end_matches('/')),
                            );
                        }
                    }
                }
                DesiredResource::AgeSecret(secret) => {
                    for candidate in [
                        secret.attributes.owner.clone().map(ResourceId::User),
                        secret.attributes.group.clone().map(ResourceId::Group),
                    ]
                    .into_iter()
                    .flatten()
                    {
                        if resources.contains_key(&candidate) {
                            depends_on.push(candidate);
                        }
                    }
                    if let Some((parent, _)) = secret.path.rsplit_once('/') {
                        let parent = if parent.is_empty() { "/" } else { parent };
                        add_managed_directory(resources, &mut depends_on, parent.into());
                    }
                }
                _ => return None,
            }
            Some((resource.id(), depends_on))
        })
        .collect();

    for (id, depends_on) in edges {
        if let Some(resource) = resources.get_mut(&id) {
            add_unique(&mut resource.depends_on, depends_on);
        }
    }
}

fn add_managed_directory(
    resources: &BTreeMap<ResourceId, Resource>,
    depends_on: &mut Vec<ResourceId>,
    path: String,
) {
    let id = ResourceId::Path(path);
    if matches!(
        resources.get(&id).map(|resource| &resource.desired),
        Some(DesiredResource::Directory(_))
    ) {
        depends_on.push(id);
    }
}

fn insert_resource(
    resources: &mut BTreeMap<ResourceId, Resource>,
    resource: Resource,
) -> Result<(), ExpansionError> {
    let id = resource.id();
    match resources.entry(id) {
        Entry::Vacant(entry) => {
            entry.insert(resource);
        }
        Entry::Occupied(mut entry) => {
            if entry.get().desired != resource.desired {
                return Err(ExpansionError(format!(
                    "conflicting resource {:?}: {} and {}",
                    entry.key(),
                    locations(entry.get()),
                    locations(&resource)
                )));
            }
            let prior = entry.get_mut();
            add_unique(&mut prior.origins, resource.origins);
            add_unique(&mut prior.depends_on, resource.depends_on);
            add_unique(&mut prior.notifications, resource.notifications);
        }
    }
    Ok(())
}

fn add_unique<T: PartialEq>(values: &mut Vec<T>, additions: impl IntoIterator<Item = T>) {
    for item in additions {
        if !values.contains(&item) {
            values.push(item);
        }
    }
}

fn check_dependencies(resources: &BTreeMap<ResourceId, Resource>) -> Result<(), ExpansionError> {
    let mut indegree = BTreeMap::new();
    let mut dependents = BTreeMap::<&ResourceId, Vec<&ResourceId>>::new();
    let mut ready = BTreeSet::new();
    for (id, resource) in resources {
        let dependencies: BTreeSet<_> = resource.depends_on.iter().collect();
        if dependencies.is_empty() {
            ready.insert(id);
        }
        indegree.insert(id, dependencies.len());
        for dependency in dependencies {
            if !resources.contains_key(dependency) {
                return Err(ExpansionError(format!(
                    "resource {id:?} at {} depends on missing resource {dependency:?}",
                    locations(resource)
                )));
            }
            dependents.entry(dependency).or_default().push(id);
        }
    }

    while let Some(id) = ready.pop_first() {
        indegree.remove(id);
        if let Some(remaining) = dependents.remove(id) {
            for dependent in remaining {
                let count = indegree.get_mut(dependent).expect("unresolved dependent");
                *count -= 1;
                if *count == 0 {
                    ready.insert(dependent);
                }
            }
        }
    }
    if !indegree.is_empty() {
        let blocked = indegree
            .keys()
            .map(|id| format!("{id:?} at {}", locations(&resources[*id])))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(ExpansionError(format!("dependency cycle blocks {blocked}")));
    }
    Ok(())
}

fn locations(resource: &Resource) -> String {
    resource
        .origins
        .iter()
        .map(|origin| format!("{} at {}", origin.module, origin.origin))
        .collect::<Vec<_>>()
        .join(", ")
}

fn digest(bytes: &[u8]) -> ContentDigest {
    ContentDigest(format!("sha256:{:x}", Sha256::digest(bytes)))
}
