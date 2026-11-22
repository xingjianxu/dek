//! Observe and atomically publish Linux regular-file resources.

mod atomic_file;
mod validation;

pub use validation::{FileValidationGroup, ValidationInput};

use std::{
    fs::File,
    io,
    path::{Component, Path, PathBuf},
};

use dek_core::{
    Action, ActionKind, ContentDigest, DesiredResource, FileAttributes, FileContent,
    FileObservation, FileResource, FileState, Precondition, ResourceCapability, ResourceKind,
    ResourceSet,
};
use dek_target::backend::{ActionContext, ActionReadiness, Backend, BackendError, PlannedAction};
use rustix::{
    fs::{Mode, Stat, fstat},
    process::{Gid, Uid},
};
use sha2::{Digest, Sha256};

use crate::accounts::{group_id, user_id};
use atomic_file::{StagedFile, TargetPath, same_revision};

#[derive(Default)]
pub struct FileBackend {
    validation: Vec<FileValidationGroup>,
}

impl FileBackend {
    pub fn with_validation(validation: Vec<FileValidationGroup>) -> Self {
        Self { validation }
    }
}

impl Backend for FileBackend {
    fn capabilities(&self) -> Vec<ResourceCapability> {
        vec![ResourceCapability {
            kind: ResourceKind::File,
            version: 1,
            attributes: ["owner", "group", "mode"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        }]
    }

    fn validate(&self, resource: &DesiredResource) -> Result<(), BackendError> {
        let file = file_resource(resource)?;
        normalize(&file.path)?;
        if let FileState::Present(content) = &file.state {
            let attributes = &content.attributes;
            mode(attributes.mode.as_deref())?;
            for name in [attributes.owner.as_deref(), attributes.group.as_deref()]
                .into_iter()
                .flatten()
            {
                if name.is_empty() || name.contains('\0') {
                    return Err(BackendError(format!("无效的账户或组名 {name:?}")));
                }
            }
        }
        Ok(())
    }

    fn observe(&self, resource: &DesiredResource) -> Result<Vec<Precondition>, BackendError> {
        let file = file_resource(resource)?;
        let attributes = match &file.state {
            FileState::Present(content) => &content.attributes,
            FileState::Absent => &FileAttributes::default(),
        };
        let observation = snapshot(&file.path, attributes)?;
        Ok(vec![Precondition::File {
            path: file.path.clone(),
            expected: observation.state,
        }])
    }

    fn plan(
        &self,
        resource: &DesiredResource,
        observed: &[Precondition],
    ) -> Result<Vec<PlannedAction>, BackendError> {
        let file = file_resource(resource)?;
        let [Precondition::File { path, expected }] = observed else {
            return Err(BackendError("文件动作缺少文件观测".into()));
        };
        if path != &file.path {
            return Err(BackendError(format!("目标路径与文件观测不一致 {path:?}")));
        }
        let (kind, reason, content_changed) = match (&file.state, expected) {
            (FileState::Present(desired), FileObservation::Present(actual))
                if desired.digest == actual.digest =>
            {
                if attributes_match(&desired.attributes, &actual.attributes)? {
                    return Ok(Vec::new());
                }
                (
                    ActionKind::SetFileAttributes {
                        path: path.clone(),
                        attributes: desired.attributes.clone(),
                    },
                    "当前文件属性与声明不同",
                    false,
                )
            }
            (FileState::Present(content), observation) => (
                ActionKind::WriteFile {
                    path: path.clone(),
                    content: content.clone(),
                },
                if *observation == FileObservation::Missing {
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
            content_changed,
            preconditions: observed.to_vec(),
        }])
    }

    fn validate_action(&self, action: &Action) -> Result<(), BackendError> {
        let change = FileAction::reviewed(action)?;
        normalize(change.path)?;
        mode(change.goal.attributes().mode.as_deref())?;
        Ok(())
    }

    fn validate_batch(
        &self,
        desired: &ResourceSet,
        actions: &[ActionContext<'_>],
    ) -> Result<(), BackendError> {
        validation::check_groups(&self.validation, desired, actions)
    }

    fn inspect_action(&self, action: &Action) -> Result<ActionReadiness, BackendError> {
        let change = FileAction::reviewed(action)?;
        let observed = snapshot(change.path, change.goal.attributes())?;
        change.readiness(&observed.state)
    }

    fn execute(&self, context: &ActionContext<'_>) -> Result<(), BackendError> {
        let change = FileAction::reviewed(context.action)?;
        let observed = snapshot(change.path, change.goal.attributes())?;
        match change.readiness(&observed.state)? {
            ActionReadiness::Satisfied => return Ok(()),
            ActionReadiness::Conflict(reason) => return Err(BackendError(reason)),
            ActionReadiness::Ready => {}
        }
        let target = observed
            .target
            .ok_or_else(|| BackendError(format!("目标父目录尚未创建 {:?}", change.path)))?;
        let problem = |err| path_error(change.path, err);

        if let Goal::Absent = change.goal {
            let expected = observed
                .stat
                .as_ref()
                .ok_or_else(|| BackendError("缺少文件观测".into()))?;
            return target.remove(expected).map_err(problem);
        }

        let digest = change.reviewed_digest()?;
        let bytes = context.content(digest)?;
        let attributes = change.goal.attributes();
        let user = requested_user(attributes.owner.as_deref())?
            .or_else(|| observed.stat.as_ref().map(|st| st.st_uid));
        let group = requested_group(attributes.group.as_deref())?
            .or_else(|| observed.stat.as_ref().map(|st| st.st_gid));
        let mode = mode(attributes.mode.as_deref())?
            .or_else(|| observed.stat.as_ref().map(|stat| stat.st_mode & 0o7777))
            .unwrap_or(0o644);
        let stage = StagedFile::new(
            &target,
            bytes,
            user.map(Uid::from_raw),
            group.map(Gid::from_raw),
            Mode::from_bits_truncate(mode),
        )
        .map_err(problem)?;
        stage.publish(observed.stat.as_ref()).map_err(problem)?;
        let after = snapshot(change.path, attributes)?;
        match after.state {
            FileObservation::Present(content)
                if content.digest == *digest
                    && attributes_match(attributes, &content.attributes)? =>
            {
                Ok(())
            }
            _ => Err(BackendError(format!(
                "文件替换后核验失败 {:?}",
                change.path
            ))),
        }
    }

    fn verify(&self, action: &ActionKind) -> Result<bool, BackendError> {
        let (path, goal) = Goal::for_kind(action)?;
        let observed = snapshot(path, goal.attributes())?;
        goal.satisfied(&observed.state, None)
    }
}

fn file_resource(resource: &DesiredResource) -> Result<&FileResource, BackendError> {
    match resource {
        DesiredResource::File(file) => Ok(file),
        _ => Err(BackendError("文件后端收到其他资源类型".into())),
    }
}

fn normalize(raw: &str) -> Result<PathBuf, BackendError> {
    let path = Path::new(raw);
    if !path.is_absolute() || raw.contains('\0') || raw.ends_with('/') || raw.ends_with("/.") {
        return Err(BackendError(format!("需要普通文件的绝对路径 {raw:?}")));
    }
    let mut normalized = PathBuf::from("/");
    for piece in path.components().skip(1) {
        match piece {
            Component::Normal(part) => normalized.push(part),
            _ => return Err(BackendError(format!("文件路径不能包含父目录组件 {raw:?}"))),
        }
    }
    Ok(normalized)
}

fn path_error(path: &str, err: io::Error) -> BackendError {
    BackendError(format!("无法访问目标文件 {path:?}：{err}"))
}

fn mode(mode: Option<&str>) -> Result<Option<u32>, BackendError> {
    mode.map(|value| {
        u32::from_str_radix(value, 8)
            .ok()
            .filter(|number| {
                *number <= 0o7777
                    && (3..=4).contains(&value.len())
                    && value.bytes().all(|byte| matches!(byte, b'0'..=b'7'))
            })
            .ok_or_else(|| BackendError(format!("无效的八进制权限 {value:?}")))
    })
    .transpose()
}

fn attributes_match(
    desired: &FileAttributes,
    observed: &FileAttributes,
) -> Result<bool, BackendError> {
    for (name, number, lookup) in [
        (
            desired.owner.as_deref(),
            observed.owner.as_deref(),
            user_id as fn(&str) -> Result<Option<u32>, BackendError>,
        ),
        (
            desired.group.as_deref(),
            observed.group.as_deref(),
            group_id,
        ),
    ] {
        if let Some(name) = name {
            let current = number.and_then(|value| value.parse().ok());
            let desired = lookup(name)?;
            if desired.is_none() || desired != current {
                return Ok(false);
            }
        }
    }
    match mode(desired.mode.as_deref())? {
        Some(value) => Ok(mode(observed.mode.as_deref())? == Some(value)),
        None => Ok(true),
    }
}

fn requested_user(name: Option<&str>) -> Result<Option<u32>, BackendError> {
    name.map(|name| user_id(name)?.ok_or_else(|| BackendError(format!("找不到属主 {name:?}"))))
        .transpose()
}

fn requested_group(name: Option<&str>) -> Result<Option<u32>, BackendError> {
    name.map(|name| group_id(name)?.ok_or_else(|| BackendError(format!("找不到属组 {name:?}"))))
        .transpose()
}

struct Snapshot {
    target: Option<TargetPath>,
    stat: Option<Stat>,
    state: FileObservation,
}

fn snapshot(raw: &str, attributes: &FileAttributes) -> Result<Snapshot, BackendError> {
    let path = normalize(raw)?;
    let problem = |err| path_error(raw, err);
    let target = TargetPath::open(&path).map_err(problem)?;
    let Some(target) = target else {
        return Ok(Snapshot {
            target: None,
            stat: None,
            state: FileObservation::Missing,
        });
    };
    let stat = target.observe().map_err(problem)?;
    let state = if let Some(stat) = &stat {
        let mut file = target.read(stat).map_err(problem)?;
        let digest = hash_bytes(&mut file).map_err(problem)?;
        if !same_revision(stat, &fstat(&file).map_err(|err| problem(err.into()))?) {
            return Err(BackendError(format!("观测期间目标文件发生变化 {raw:?}")));
        }
        FileObservation::Present(FileContent {
            digest,
            attributes: FileAttributes {
                owner: attributes.owner.as_ref().map(|_| stat.st_uid.to_string()),
                group: attributes.group.as_ref().map(|_| stat.st_gid.to_string()),
                mode: attributes
                    .mode
                    .as_ref()
                    .map(|_| format!("{:04o}", stat.st_mode & 0o7777)),
            },
        })
    } else {
        FileObservation::Missing
    };
    Ok(Snapshot {
        target: Some(target),
        stat,
        state,
    })
}

fn hash_bytes(file: &mut File) -> io::Result<ContentDigest> {
    let mut sha = Sha256::new();
    io::copy(file, &mut sha)?;
    Ok(ContentDigest(format!("sha256:{:x}", sha.finalize())))
}

fn digest(bytes: &[u8]) -> ContentDigest {
    ContentDigest(format!("sha256:{:x}", Sha256::digest(bytes)))
}

struct FileAction<'a> {
    path: &'a str,
    goal: Goal<'a>,
    expected: &'a FileObservation,
}

impl<'a> FileAction<'a> {
    fn reviewed(action: &'a Action) -> Result<Self, BackendError> {
        let (path, goal) = Goal::for_kind(&action.kind)?;
        let [
            Precondition::File {
                path: reviewed,
                expected,
            },
        ] = action.preconditions.as_slice()
        else {
            return Err(BackendError(format!("文件动作缺少前置条件 {path:?}")));
        };
        if path != reviewed {
            return Err(BackendError(format!("动作与前置条件的路径不符 {path:?}")));
        }
        Ok(Self {
            path,
            goal,
            expected,
        })
    }

    fn readiness(&self, actual: &FileObservation) -> Result<ActionReadiness, BackendError> {
        if self.goal.satisfied(actual, Some(self.expected))? {
            return Ok(ActionReadiness::Satisfied);
        }
        if actual != self.expected {
            return Ok(ActionReadiness::Conflict(format!(
                "目标文件在审阅后改变 {:?}",
                self.path
            )));
        }
        Ok(ActionReadiness::Ready)
    }

    fn reviewed_digest(&self) -> Result<&ContentDigest, BackendError> {
        match (&self.goal, self.expected) {
            (Goal::Write(content), _) => Ok(&content.digest),
            (Goal::Attributes(_), FileObservation::Present(content)) => Ok(&content.digest),
            _ => Err(BackendError(format!(
                "缺少目标文件的已审阅内容 {:?}",
                self.path
            ))),
        }
    }
}

enum Goal<'a> {
    Write(&'a FileContent),
    Attributes(&'a FileAttributes),
    Absent,
}

impl<'a> Goal<'a> {
    fn for_kind(kind: &'a ActionKind) -> Result<(&'a str, Self), BackendError> {
        match kind {
            ActionKind::WriteFile { path, content } => Ok((path, Self::Write(content))),
            ActionKind::SetFileAttributes { path, attributes } => {
                Ok((path, Self::Attributes(attributes)))
            }
            ActionKind::RemoveFile { path } => Ok((path, Self::Absent)),
            _ => Err(BackendError("文件后端收到其他动作类型".into())),
        }
    }

    fn attributes(&self) -> &FileAttributes {
        static UNDECLARED: FileAttributes = FileAttributes {
            owner: None,
            group: None,
            mode: None,
        };
        match self {
            Self::Write(content) => &content.attributes,
            Self::Attributes(attributes) => attributes,
            Self::Absent => &UNDECLARED,
        }
    }

    fn satisfied(
        &self,
        state: &FileObservation,
        expected: Option<&FileObservation>,
    ) -> Result<bool, BackendError> {
        match (self, state) {
            (Self::Absent, FileObservation::Missing) => Ok(true),
            (Self::Write(wanted), FileObservation::Present(actual)) => Ok(wanted.digest
                == actual.digest
                && attributes_match(&wanted.attributes, &actual.attributes)?),
            (Self::Attributes(wanted), FileObservation::Present(actual)) => {
                let same_content = match expected {
                    Some(FileObservation::Present(reviewed)) => reviewed.digest == actual.digest,
                    Some(FileObservation::Missing) => false,
                    None => true,
                };
                Ok(same_content && attributes_match(wanted, &actual.attributes)?)
            }
            _ => Ok(false),
        }
    }
}
