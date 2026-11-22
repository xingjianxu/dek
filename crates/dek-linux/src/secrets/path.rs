use std::{
    ffi::OsString,
    fs::{File, Metadata},
    io,
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
};

use dek_target::backend::BackendError;
use rustix::{
    fd::OwnedFd,
    fs::{FileType, Mode, OFlags, Stat, fstat, open, openat},
    io::Errno,
};

pub(super) struct CheckedPath {
    pub(super) normalized: PathBuf,
    pub(super) file: Option<ExistingFile>,
}

pub(super) struct ExistingFile {
    parent: OwnedFd,
    name: OsString,
    pub(super) stat: Stat,
}

pub(super) fn normalized_absolute(raw: &str) -> Result<PathBuf, BackendError> {
    let path = Path::new(raw);
    if !path.is_absolute() || raw.contains('\0') || raw.ends_with('/') || raw.ends_with("/.") {
        return Err(BackendError(format!(
            "加密文件和 identity 需要绝对文件路径 {raw:?}"
        )));
    }
    let mut normalized = PathBuf::from("/");
    for component in path.components().skip(1) {
        let Component::Normal(name) = component else {
            return Err(BackendError(format!("路径不得包含父目录组件 {raw:?}")));
        };
        normalized.push(name);
    }
    if normalized == Path::new("/") {
        return Err(BackendError(format!("路径需要文件名 {raw:?}")));
    }
    Ok(normalized)
}

impl CheckedPath {
    pub(super) fn identity(raw: &str) -> Result<Self, BackendError> {
        Self::inspect_path(raw, true)
    }

    /// Keep each parent open while resolving its child without following links.
    pub(super) fn inspect(raw: &str) -> Result<Self, BackendError> {
        Self::inspect_path(raw, false)
    }

    fn inspect_path(raw: &str, protected_identity: bool) -> Result<Self, BackendError> {
        let normalized = normalized_absolute(raw)?;
        let flags = OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let problem = |err| BackendError(format!("检查加密文件或 identity 路径 {raw:?}：{err}"));
        let mut parent = open("/", flags | OFlags::DIRECTORY, Mode::empty()).map_err(problem)?;
        let mut parts = normalized.iter().skip(1).peekable();
        while let Some(part) = parts.next() {
            let fd = match openat(&parent, part, flags, Mode::empty()) {
                Ok(fd) => fd,
                Err(err) if err == Errno::NOENT || (protected_identity && err == Errno::ACCESS) => {
                    return Ok(Self {
                        normalized,
                        file: None,
                    });
                }
                Err(err) => return Err(problem(err)),
            };
            let stat = fstat(&fd).map_err(problem)?;
            let last = parts.peek().is_none();
            match (FileType::from_raw_mode(stat.st_mode), last) {
                (FileType::Symlink, _) => {
                    return Err(BackendError(format!("路径不得经过符号链接 {raw:?}")));
                }
                (FileType::Directory, false) => parent = fd,
                (FileType::RegularFile, true) => {
                    let name = part.to_owned();
                    return Ok(Self {
                        normalized,
                        file: Some(ExistingFile { parent, name, stat }),
                    });
                }
                _ => return Err(BackendError(format!("路径类型与普通文件不符 {raw:?}"))),
            }
        }
        unreachable!("an absolute file path has at least one normal component")
    }

    pub(super) fn aliases(&self, other: &Self) -> bool {
        self.normalized == other.normalized
            || self
                .file
                .as_ref()
                .zip(other.file.as_ref())
                .is_some_and(|(first, second)| first.same_inode(&second.stat))
    }
}

impl ExistingFile {
    fn same_inode(&self, other: &Stat) -> bool {
        self.stat.st_dev == other.st_dev && self.stat.st_ino == other.st_ino
    }

    pub(super) fn matches_metadata(&self, other: &Metadata) -> bool {
        self.stat.st_dev == other.dev() && self.stat.st_ino == other.ino()
    }

    pub(super) fn open(&self) -> io::Result<File> {
        let fd = openat(
            &self.parent,
            &self.name,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let stat = fstat(&fd)?;
        if !self.same_inode(&stat) || FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
        {
            return Err(io::Error::other("观测期间路径已变化"));
        }
        Ok(File::from(fd))
    }

    pub(super) fn unchanged(&self, file: &File) -> io::Result<bool> {
        let stat = fstat(file)?;
        Ok(self.same_inode(&stat)
            && self.stat.st_ctime == stat.st_ctime
            && self.stat.st_ctime_nsec == stat.st_ctime_nsec
            && self.stat.st_mtime == stat.st_mtime
            && self.stat.st_mtime_nsec == stat.st_mtime_nsec
            && self.stat.st_size == stat.st_size)
    }
}
