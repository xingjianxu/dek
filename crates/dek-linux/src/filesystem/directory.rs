use std::{
    ffi::OsStr,
    io,
    os::fd::AsRawFd,
    path::{Component, Path, PathBuf},
};

use dek_core::{
    Action, ActionKind, DesiredResource, DirectoryObservation, DirectoryResource, FileAttributes,
    Precondition, ResourceCapability, ResourceId, ResourceKind, ResourceSet,
};
use dek_target::backend::{ActionContext, ActionReadiness, Backend, BackendError, PlannedAction};
use rustix::{
    fd::{AsFd, BorrowedFd, OwnedFd},
    fs::{
        AtFlags, FileType, Mode, OFlags, RenameFlags, Stat, chownat, fchmod, fchown, fstat, fsync,
        mkdirat, openat, renameat_with, statat, unlinkat,
    },
    io::Errno,
    process::{Gid, Uid},
};

use crate::accounts::{group_id, user_id};

use super::{
    atomic_file::{StagingDirectory, TargetPath, same_inode},
    mode, requested_group, requested_user, validate_attributes,
};

pub struct DirectoryBackend;

impl Backend for DirectoryBackend {
    fn capabilities(&self) -> Vec<ResourceCapability> {
        vec![ResourceCapability {
            kind: ResourceKind::Directory,
            version: 1,
            attributes: ["owner", "group", "mode"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        }]
    }

    fn validate(&self, resource: &DesiredResource) -> Result<(), BackendError> {
        validate(resource_directory(resource)?)
    }

    fn resource_dependencies(
        &self,
        resource: &DesiredResource,
        managed: &ResourceSet,
    ) -> Result<Vec<ResourceId>, BackendError> {
        let directory = resource_directory(resource)?;
        let mut dependencies = Vec::new();
        for (name, id, lookup) in [
            (
                directory.attributes.owner.as_ref(),
                ResourceId::User as fn(String) -> ResourceId,
                user_id as fn(&str) -> Result<Option<u32>, BackendError>,
            ),
            (
                directory.attributes.group.as_ref(),
                ResourceId::Group,
                group_id,
            ),
        ] {
            if let Some(name) = name {
                let identity = id(name.clone());
                if managed.resources.iter().any(|entry| entry.id() == identity) {
                    dependencies.push(identity);
                } else if lookup(name)?.is_none() {
                    return Err(BackendError(format!(
                        "目录属性引用的账户或组缺失 {identity:?}"
                    )));
                }
            }
        }
        let path = normalize(&directory.path)?;
        for resource in &managed.resources {
            if let DesiredResource::Directory(parent) = &resource.desired {
                let prefix = normalize(&parent.path)?;
                if prefix != path && path.starts_with(prefix) {
                    dependencies.push(resource.id());
                }
            }
        }
        Ok(dependencies)
    }

    fn observe(&self, resource: &DesiredResource) -> Result<Vec<Precondition>, BackendError> {
        let directory = resource_directory(resource)?;
        let observed = snapshot(directory)?;
        Ok(vec![Precondition::Directory {
            path: directory.path.clone(),
            expected: observed
                .stat
                .as_ref()
                .map(|stat| metadata(stat, &directory.attributes)),
        }])
    }

    fn plan(
        &self,
        resource: &DesiredResource,
        observed: &[Precondition],
    ) -> Result<Vec<PlannedAction>, BackendError> {
        let directory = resource_directory(resource)?;
        let expected = expected(directory, observed)?;
        let reason = match expected {
            Some(actual) if matches(&directory.attributes, actual)? => return Ok(Vec::new()),
            Some(_) => "当前目录属性与声明不同",
            None => "目标目录缺失",
        };
        Ok(vec![PlannedAction {
            kind: ActionKind::EnsureDirectory(directory.clone()),
            reason: reason.into(),
            preconditions: observed.to_vec(),
            content_changed: false,
        }])
    }

    fn validate_action(&self, action: &Action) -> Result<(), BackendError> {
        let directory = action_directory(&action.kind)?;
        expected(directory, &action.preconditions)?;
        validate(directory)
    }

    fn inspect_action(&self, action: &Action) -> Result<ActionReadiness, BackendError> {
        let directory = action_directory(&action.kind)?;
        readiness(
            directory,
            &action.preconditions,
            snapshot(directory)?.stat.as_ref(),
        )
    }

    fn execute(&self, context: &ActionContext<'_>) -> Result<(), BackendError> {
        let directory = action_directory(&context.action.kind)?;
        let before = snapshot(directory)?;
        match readiness(
            directory,
            &context.action.preconditions,
            before.stat.as_ref(),
        )? {
            ActionReadiness::Satisfied => return Ok(()),
            ActionReadiness::Conflict(reason) => return Err(BackendError(reason)),
            ActionReadiness::Ready => {}
        }
        let target = before
            .target
            .ok_or_else(|| BackendError(format!("目标父目录尚未创建 {:?}", directory.path)))?;
        let desired = &directory.attributes;
        let attributes = Attributes {
            uid: requested_user(desired.owner.as_deref())?,
            gid: requested_group(desired.group.as_deref())?,
            mode: mode(desired.mode.as_deref())?,
        };
        let problem = |err| path_error(directory, err);

        let updated = if let Some(observed) = before.stat {
            let fd = DirectoryFd::open(&target.parent, &target.name).map_err(problem)?;
            let actual = fstat(&fd).map_err(|err| problem(err.into()))?;
            if metadata(&actual, desired) != metadata(&observed, desired) {
                return Err(BackendError(conflict(directory)));
            }
            let result = set_attributes(&fd, &actual, &attributes).map_err(problem)?;
            fd.sync().map_err(problem)?;
            result
        } else {
            create(&target, &attributes).map_err(problem)?
        };
        let after = snapshot(directory)?;
        if after
            .stat
            .is_some_and(|actual| same_inode(&updated, &actual) && attributes.match_stat(&actual))
        {
            Ok(())
        } else {
            Err(BackendError(format!(
                "目录更新后核验失败 {:?}",
                directory.path
            )))
        }
    }

    fn verify(&self, action: &ActionKind) -> Result<bool, BackendError> {
        let directory = action_directory(action)?;
        match snapshot(directory)?.stat {
            Some(actual) => matches(
                &directory.attributes,
                &metadata(&actual, &directory.attributes),
            ),
            None => Ok(false),
        }
    }
}

fn resource_directory(resource: &DesiredResource) -> Result<&DirectoryResource, BackendError> {
    match resource {
        DesiredResource::Directory(directory) => Ok(directory),
        _ => Err(BackendError("目录后端收到其他资源类型".into())),
    }
}

fn action_directory(action: &ActionKind) -> Result<&DirectoryResource, BackendError> {
    match action {
        ActionKind::EnsureDirectory(directory) => Ok(directory),
        _ => Err(BackendError("目录后端收到其他动作类型".into())),
    }
}

fn validate(directory: &DirectoryResource) -> Result<(), BackendError> {
    normalize(&directory.path)?;
    validate_attributes(&directory.attributes)
}

fn normalize(raw: &str) -> Result<PathBuf, BackendError> {
    let path = Path::new(raw);
    if !path.is_absolute() || raw.contains('\0') {
        return Err(BackendError(format!("需要目录的绝对路径 {raw:?}")));
    }
    let mut normalized = PathBuf::from("/");
    for piece in path.components().skip(1) {
        match piece {
            Component::Normal(part) => normalized.push(part),
            _ => return Err(BackendError(format!("目录路径不能包含父目录组件 {raw:?}"))),
        }
    }
    Ok(normalized)
}

fn expected<'a>(
    directory: &DirectoryResource,
    preconditions: &'a [Precondition],
) -> Result<Option<&'a DirectoryObservation>, BackendError> {
    match preconditions {
        [Precondition::Directory { path, expected }] if path == &directory.path => {
            Ok(expected.as_ref())
        }
        _ => Err(BackendError(format!(
            "目录动作缺少对应路径的前置条件 {:?}",
            directory.path
        ))),
    }
}

fn conflict(directory: &DirectoryResource) -> String {
    format!("目标目录在审阅后改变 {:?}", directory.path)
}

fn readiness(
    directory: &DirectoryResource,
    preconditions: &[Precondition],
    actual: Option<&Stat>,
) -> Result<ActionReadiness, BackendError> {
    let reviewed = expected(directory, preconditions)?;
    let observed = actual.map(|actual| metadata(actual, &directory.attributes));
    if let (Some(reviewed), Some(observed)) = (reviewed, observed.as_ref())
        && (reviewed.device, reviewed.inode) != (observed.device, observed.inode)
    {
        return Ok(ActionReadiness::Conflict(conflict(directory)));
    }
    if let Some(actual) = observed.as_ref()
        && matches(&directory.attributes, actual)?
    {
        return Ok(ActionReadiness::Satisfied);
    }
    if reviewed != observed.as_ref() {
        return Ok(ActionReadiness::Conflict(conflict(directory)));
    }
    Ok(ActionReadiness::Ready)
}

fn path_error(directory: &DirectoryResource, err: io::Error) -> BackendError {
    let kind = match err.raw_os_error() {
        Some(libc::ENOTDIR | libc::ELOOP) => "目录路径类型冲突",
        _ => "无法访问目标目录",
    };
    BackendError(format!("{kind} {:?}：{err}", directory.path))
}

struct Snapshot {
    target: Option<TargetPath>,
    stat: Option<Stat>,
}

fn snapshot(directory: &DirectoryResource) -> Result<Snapshot, BackendError> {
    let path = normalize(&directory.path)?;
    let target = TargetPath::open(&path).map_err(|error| path_error(directory, error))?;
    let stat = match &target {
        Some(target) => match statat(&target.parent, &target.name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::Directory => Some(stat),
            Ok(_) => {
                return Err(BackendError(format!(
                    "目录路径类型冲突 {:?}",
                    directory.path
                )));
            }
            Err(Errno::NOENT) => None,
            Err(err) => return Err(path_error(directory, err.into())),
        },
        None => None,
    };
    Ok(Snapshot { target, stat })
}

fn metadata(stat: &Stat, declaration: &FileAttributes) -> DirectoryObservation {
    DirectoryObservation {
        device: stat.st_dev,
        inode: stat.st_ino,
        uid: declaration.owner.as_ref().map(|_| stat.st_uid),
        gid: declaration.group.as_ref().map(|_| stat.st_gid),
        mode: declaration.mode.as_ref().map(|_| stat.st_mode & 0o7777),
    }
}

fn matches(
    declaration: &FileAttributes,
    observation: &DirectoryObservation,
) -> Result<bool, BackendError> {
    if let Some(name) = &declaration.owner
        && (observation.uid.is_none() || user_id(name)? != observation.uid)
    {
        return Ok(false);
    }
    if let Some(name) = &declaration.group
        && (observation.gid.is_none() || group_id(name)? != observation.gid)
    {
        return Ok(false);
    }
    Ok(declaration.mode.is_none() || mode(declaration.mode.as_deref())? == observation.mode)
}

enum DirectoryFd {
    Readable(OwnedFd),
    Metadata(OwnedFd),
}

impl DirectoryFd {
    fn open(parent: &OwnedFd, name: &OsStr) -> io::Result<Self> {
        let flags = OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        match openat(parent, name, flags | OFlags::RDONLY, Mode::empty()) {
            Ok(fd) => Ok(Self::Readable(fd)),
            // Owners can repair permissions without permission to read the directory.
            Err(Errno::ACCESS) => Ok(Self::Metadata(openat(
                parent,
                name,
                flags | OFlags::PATH,
                Mode::empty(),
            )?)),
            Err(err) => Err(err.into()),
        }
    }

    fn chown(&self, uid: Option<Uid>, gid: Option<Gid>) -> io::Result<()> {
        match self {
            Self::Readable(fd) => Ok(fchown(fd, uid, gid)?),
            Self::Metadata(fd) => Ok(chownat(fd, "", uid, gid, AtFlags::EMPTY_PATH)?),
        }
    }

    fn chmod(&self, mode: Mode) -> io::Result<()> {
        match self {
            Self::Readable(fd) => Ok(fchmod(fd, mode)?),
            Self::Metadata(fd) => {
                // SAFETY: the owned O_PATH descriptor pins the directory inode.
                // AT_EMPTY_PATH acts on that descriptor using a valid empty C string.
                let result = unsafe {
                    libc::fchmodat(
                        fd.as_raw_fd(),
                        c"".as_ptr(),
                        mode.bits(),
                        libc::AT_EMPTY_PATH,
                    )
                };
                if result < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            }
        }
    }

    fn sync(&self) -> io::Result<()> {
        match self {
            Self::Readable(fd) => Ok(fsync(fd)?),
            // Reopen the pinned inode if the new permissions allow reading it.
            Self::Metadata(fd) => {
                let read = openat(
                    fd,
                    ".",
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                );
                match read {
                    Ok(directory) => Ok(fsync(&directory)?),
                    Err(Errno::ACCESS) => Ok(()),
                    Err(err) => Err(err.into()),
                }
            }
        }
    }
}

impl AsFd for DirectoryFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        match self {
            Self::Readable(fd) | Self::Metadata(fd) => fd.as_fd(),
        }
    }
}

struct Attributes {
    uid: Option<u32>,
    gid: Option<u32>,
    mode: Option<u32>,
}

impl Attributes {
    fn match_stat(&self, actual: &Stat) -> bool {
        self.uid.is_none_or(|uid| actual.st_uid == uid)
            && self.gid.is_none_or(|gid| actual.st_gid == gid)
            && self.mode.is_none_or(|mode| actual.st_mode & 0o7777 == mode)
    }
}

fn set_attributes(fd: &DirectoryFd, initial: &Stat, attrs: &Attributes) -> io::Result<Stat> {
    let uid = attrs.uid.filter(|uid| *uid != initial.st_uid);
    let gid = attrs.gid.filter(|gid| *gid != initial.st_gid);
    let current = if uid.is_some() || gid.is_some() {
        fd.chown(uid.map(Uid::from_raw), gid.map(Gid::from_raw))?;
        fstat(fd)?
    } else {
        *initial
    };
    // chown can clear special permission bits even when mode is undeclared.
    let mode = attrs.mode.unwrap_or(initial.st_mode & 0o7777);
    let updated = if current.st_mode & 0o7777 != mode {
        fd.chmod(Mode::from_bits_truncate(mode))?;
        fstat(fd)?
    } else {
        current
    };
    if updated.st_mode & 0o7777 != mode
        || updated.st_uid != attrs.uid.unwrap_or(initial.st_uid)
        || updated.st_gid != attrs.gid.unwrap_or(initial.st_gid)
    {
        return Err(io::Error::other(
            "directory attributes did not match after update",
        ));
    }
    Ok(updated)
}

fn create(target: &TargetPath, attrs: &Attributes) -> io::Result<Stat> {
    let stage = StagingDirectory::new(target)?;
    let name = "directory";
    mkdirat(&stage.directory, name, Mode::from_bits_truncate(0o755))?;
    let result = (|| {
        let fd = DirectoryFd::open(&stage.directory, OsStr::new(name))?;
        let initial = fstat(&fd)?;
        let updated = set_attributes(&fd, &initial, attrs)?;
        fd.sync()?;
        renameat_with(
            &stage.directory,
            name,
            &target.parent,
            &target.name,
            RenameFlags::NOREPLACE,
        )?;
        fsync(&target.parent)?;
        Ok(updated)
    })();
    if result.is_err() {
        let _ = unlinkat(&stage.directory, name, AtFlags::REMOVEDIR);
    }
    result
}
