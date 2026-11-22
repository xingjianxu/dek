//! Read-only filesystem observations for local planning.

use std::{
    env,
    error::Error,
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Read},
    path::Path,
};

use dek_controller::eval::{TargetAnswer, TargetProvider};
use dek_core::{
    ActionKind, ContentDigest, DesiredResource, FileAttributes, FileContent, FileObservation,
    FileResource, FileState, Observation, Precondition, ResourceCapability, ResourceKind,
    TargetIdentity,
    protocol::{TargetQuery, TargetQueryKind},
};
use dek_target::backend::{Backend, BackendError, BackendRegistry, PlannedAction};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub struct LocalFileBackend;

pub fn local_registry(backend: &LocalFileBackend) -> Result<BackendRegistry<'_>, Box<dyn Error>> {
    let id = hostname::get()?
        .into_string()
        .map_err(|name| format!("当前主机名不是有效的 UTF-8：{name:?}"))?;
    let distribution = if cfg!(target_os = "linux") {
        fs::read_to_string("/etc/os-release")
            .ok()
            .and_then(|contents| {
                contents
                    .lines()
                    .find_map(|line| line.strip_prefix("ID="))
                    .map(|id| id.trim_matches(['"', '\'']).to_owned())
            })
    } else {
        None
    };
    let mut registry = BackendRegistry::new(
        TargetIdentity {
            id,
            os: env::consts::OS.into(),
            distribution,
            architecture: env::consts::ARCH.into(),
        },
        1,
        normalize_path,
    );
    registry.register(backend)?;
    Ok(registry)
}

fn normalize_path(raw: &str) -> Result<String, BackendError> {
    let path = absolute_path(raw)?;
    let problem = |err| BackendError(format!("cannot resolve {raw:?}: {err}"));
    let resolved = match path.canonicalize() {
        Ok(resolved) => resolved,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            let name = path.file_name().ok_or_else(|| {
                BackendError(format!("cannot determine the file name of {raw:?}"))
            })?;
            path.parent()
                .expect("absolute path with file name has a parent")
                .canonicalize()
                .map_err(problem)?
                .join(name)
        }
        Err(err) => return Err(problem(err)),
    };
    resolved
        .into_os_string()
        .into_string()
        .map_err(|_| BackendError(format!("resolved path for {raw:?} is not UTF-8")))
}

fn absolute_path(raw: &str) -> Result<&Path, BackendError> {
    let path = Path::new(raw);
    if !path.is_absolute() || raw.contains('\0') {
        return Err(BackendError(format!(
            "expected an absolute target path: {raw:?}"
        )));
    }
    Ok(path)
}

impl Backend for LocalFileBackend {
    fn capabilities(&self) -> Vec<ResourceCapability> {
        let mode = cfg!(unix).then_some("mode".to_owned());
        vec![ResourceCapability {
            kind: ResourceKind::File,
            version: 1,
            attributes: mode.into_iter().collect(),
        }]
    }

    fn validate(&self, resource: &DesiredResource) -> Result<(), BackendError> {
        let file = file_resource(resource)?;
        absolute_path(&file.path)?;
        if let FileState::Present(content) = &file.state {
            file_mode(content.attributes.mode.as_deref())?;
        }
        Ok(())
    }

    fn observe(&self, resource: &DesiredResource) -> Result<Vec<Precondition>, BackendError> {
        let file = file_resource(resource)?;
        let path = Path::new(&file.path);
        let read_error = |err| BackendError(format!("cannot observe {}: {err}", path.display()));
        let state = match open_regular(path, false).map_err(read_error)? {
            Some((mut file_handle, metadata)) => {
                let mut hash = Sha256::new();
                io::copy(&mut file_handle, &mut hash).map_err(read_error)?;
                let mode = match &file.state {
                    FileState::Present(content) if content.attributes.mode.is_some() => {
                        observe_mode(&metadata)
                    }
                    _ => None,
                };
                FileObservation::Present(FileContent {
                    digest: ContentDigest(format!("sha256:{:x}", hash.finalize())),
                    attributes: FileAttributes {
                        mode,
                        ..FileAttributes::default()
                    },
                })
            }
            None => FileObservation::Missing,
        };
        Ok(vec![Precondition::File {
            path: file.path.clone(),
            expected: state,
        }])
    }

    fn plan(
        &self,
        resource: &DesiredResource,
        observed: &[Precondition],
    ) -> Result<Vec<PlannedAction>, BackendError> {
        let file = file_resource(resource)?;
        let [Precondition::File { path, expected }] = observed else {
            return Err(BackendError("expected one file observation".into()));
        };
        if path != &file.path {
            return Err(BackendError(format!(
                "unexpected file observation for {path:?}"
            )));
        }

        let (kind, reason, content_changed) = match (&file.state, expected) {
            (FileState::Present(desired), FileObservation::Present(actual))
                if desired.digest == actual.digest =>
            {
                if desired.attributes.mode.is_none()
                    || file_mode(desired.attributes.mode.as_deref())?
                        == file_mode(actual.attributes.mode.as_deref())?
                {
                    return Ok(Vec::new());
                }
                (
                    ActionKind::SetFileAttributes {
                        path: path.clone(),
                        attributes: desired.attributes.clone(),
                    },
                    "当前文件权限与声明不同",
                    false,
                )
            }
            (FileState::Present(content), observed) => (
                ActionKind::WriteFile {
                    path: path.clone(),
                    content: content.clone(),
                },
                if observed == &FileObservation::Missing {
                    "目标文件缺失"
                } else {
                    "当前文件内容与声明不同"
                },
                true,
            ),
            (FileState::Absent, FileObservation::Present(_)) => (
                ActionKind::RemoveFile { path: path.clone() },
                "已声明移除目标文件",
                true,
            ),
            (FileState::Absent, FileObservation::Missing) => return Ok(Vec::new()),
        };
        Ok(vec![PlannedAction {
            kind,
            reason: reason.into(),
            preconditions: observed.to_vec(),
            content_changed,
        }])
    }

    fn execute(&self, _: &ActionKind) -> Result<(), BackendError> {
        Err(BackendError(
            "local file planner supports read-only observations".into(),
        ))
    }

    fn verify(&self, _: &ActionKind) -> Result<bool, BackendError> {
        Err(BackendError(
            "local file planner supports read-only observations".into(),
        ))
    }
}

fn file_resource(resource: &DesiredResource) -> Result<&FileResource, BackendError> {
    match resource {
        DesiredResource::File(file) => Ok(file),
        _ => Err(BackendError(
            "local file planner expects a file resource".into(),
        )),
    }
}

fn file_mode(mode: Option<&str>) -> Result<Option<u32>, BackendError> {
    mode.map(|mode| {
        if !(3..=4).contains(&mode.len()) || !mode.bytes().all(|digit| matches!(digit, b'0'..=b'7'))
        {
            return Err(BackendError(format!("invalid octal file mode {mode:?}")));
        }
        u32::from_str_radix(mode, 8).map_err(|err| BackendError(err.to_string()))
    })
    .transpose()
}

#[cfg(unix)]
fn observe_mode(metadata: &Metadata) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    Some(format!("{:04o}", metadata.permissions().mode() & 0o7777))
}

#[cfg(not(unix))]
fn observe_mode(_: &Metadata) -> Option<String> {
    None
}

fn open_regular(path: &Path, follow_links: bool) -> io::Result<Option<(File, Metadata)>> {
    let metadata = if follow_links {
        fs::metadata(path)
    } else {
        fs::symlink_metadata(path)
    };
    let metadata = match metadata {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    ensure_regular(&metadata)?;

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let flags = libc::O_NONBLOCK | if follow_links { 0 } else { libc::O_NOFOLLOW };
        options.custom_flags(flags);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    ensure_regular(&metadata)?;
    Ok(Some((file, metadata)))
}

fn ensure_regular(metadata: &Metadata) -> io::Result<()> {
    if metadata.is_file() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a regular file",
        ))
    }
}

pub struct LocalEvaluationTarget<'a, 'registry>(pub &'a BackendRegistry<'registry>);

impl TargetProvider for LocalEvaluationTarget<'_, '_> {
    fn query(&mut self, query: &TargetQuery) -> Result<TargetAnswer, String> {
        let target = self.0.target();
        let value = match query.kind {
            TargetQueryKind::Fact => match query.key.as_str() {
                "architecture" => json!(target.architecture),
                "os" => json!(target.os),
                "distribution" => json!(target.distribution),
                "id" => json!(target.id),
                _ => Value::Null,
            },
            TargetQueryKind::Capability => {
                let supported = self.0.capabilities().resources.iter().any(|capability| {
                    json!(capability.kind).as_str().is_some_and(|kind| {
                        query.key == kind
                            || query
                                .key
                                .strip_prefix(kind)
                                .and_then(|suffix| suffix.strip_prefix('.'))
                                .is_some_and(|attribute| capability.attributes.contains(attribute))
                    })
                });
                json!(supported)
            }
            TargetQueryKind::ReadText => {
                let path = absolute_path(&query.key).map_err(|err| err.to_string())?;
                let Some((mut file, _)) = open_regular(path, true)
                    .map_err(|err| format!("cannot read target file {}: {err}", path.display()))?
                else {
                    return Ok(TargetAnswer {
                        value: Value::Null,
                        observation: Observation::Missing,
                    });
                };
                let mut content = String::new();
                file.read_to_string(&mut content)
                    .map_err(|err| format!("cannot read target file {}: {err}", path.display()))?;
                let observation =
                    Observation::Digest(format!("sha256:{:x}", Sha256::digest(content.as_bytes())));
                return Ok(TargetAnswer {
                    value: Value::String(content),
                    observation,
                });
            }
        };
        let observation = if value.is_null() {
            Observation::Missing
        } else {
            let encoded = serde_json::to_vec(&value).map_err(|err| err.to_string())?;
            Observation::Digest(format!("sha256:{:x}", Sha256::digest(&encoded)))
        };
        Ok(TargetAnswer { value, observation })
    }
}
