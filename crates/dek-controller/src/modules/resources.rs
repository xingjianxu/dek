use std::{
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
    fs,
    path::{Component, Path},
};

use dek_core::{
    ContentDigest, DesiredResource, FileAttributes, FileContent, FileResource, FileState,
    InputKind, InputObservation, Observation, Origin, Resource, ResourceId, ResourceOrigin,
    ResourceSet, ServiceNotification,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{ExpansionError, ModuleExpansion};

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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum ResourceDraft {
    Ready(DesiredResource),
    File {
        path: String,
        #[serde(default)]
        attributes: FileAttributes,
        content: FileSource,
    },
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
        for (address, bytes) in &expansion.output.contents {
            if *address != digest(bytes) {
                return Err(ExpansionError(format!(
                    "expand {} at {} returned bytes that do not match {}",
                    expansion.definition.name, expansion.definition.origin, address.0
                )));
            }
            contents.insert(address.clone(), bytes.clone());
        }
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
    let desired = match &declaration.desired {
        ResourceDraft::Ready(resource) => resource.clone(),
        ResourceDraft::File {
            path,
            attributes,
            content,
        } => {
            let address = match content {
                FileSource::Inline(bytes) => {
                    let address = digest(bytes);
                    contents.insert(address.clone(), bytes.clone());
                    address
                }
                FileSource::Relative { path, origin } => {
                    let relative = Path::new(path);
                    if path.is_empty()
                        || relative.components().any(|component| {
                            matches!(component, Component::Prefix(_) | Component::RootDir)
                        })
                    {
                        return Err(ExpansionError(format!(
                            "source file {path:?} at {origin} must be relative to the declaration"
                        )));
                    }
                    let file = repo_root.join(origin.file()).with_file_name(relative);
                    let bytes = fs::metadata(&file)
                        .and_then(|metadata| {
                            if !metadata.is_file() {
                                return Err(std::io::Error::new(
                                    std::io::ErrorKind::InvalidInput,
                                    "source is not a regular file",
                                ));
                            }
                            fs::read(&file)
                        })
                        .map_err(|err| {
                            ExpansionError(format!(
                                "cannot read source file {} for {} at {origin}: {err}",
                                file.display(),
                                expansion.definition.name
                            ))
                        })?;
                    let address = digest(&bytes);
                    contents.insert(address.clone(), bytes);
                    inputs.push(InputObservation {
                        kind: InputKind::File,
                        id: file.to_string_lossy().into_owned(),
                        observation: Observation::Digest(address.0.clone()),
                        origin: Some(origin.clone()),
                    });
                    if !origins.contains(origin) {
                        origins.push(origin.clone());
                    }
                    address
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

fn add_unique<T: PartialEq>(values: &mut Vec<T>, additions: Vec<T>) {
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
