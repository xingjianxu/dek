use std::{
    ffi::OsStr,
    fs::File,
    io::{self, Read, Write},
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use rustix::{
    fs::{Dir, Mode, OFlags, RenameFlags, fchmod, mkdirat, openat, renameat_with, unlinkat},
    process::geteuid,
};

use super::StateError;

pub(super) struct Directory {
    pub(super) path: PathBuf,
    fd: File,
}

impl Directory {
    pub(super) fn open_root(path: &Path) -> Result<Self, StateError> {
        Self::walk_root(path, true)
    }

    pub(super) fn existing_root(path: &Path) -> Result<Option<Self>, StateError> {
        optional(Self::walk_root(path, false))
    }

    pub(super) fn existing_private_child(&self, name: &str) -> Result<Option<Self>, StateError> {
        optional(self.open_child(OsStr::new(name), true))
    }

    fn walk_root(path: &Path, create: bool) -> Result<Self, StateError> {
        if !path.is_absolute() || path == Path::new("/") {
            return Err(StateError::invalid(path, "需要绝对路径及私有状态目录"));
        }

        let mut current = Self {
            path: "/".into(),
            fd: File::open("/").map_err(|err| StateError::io("打开目录", Path::new("/"), err))?,
        };
        let mut parts = path.components().skip(1).peekable();
        while let Some(part) = parts.next() {
            let Component::Normal(name) = part else {
                return Err(StateError::invalid(path, "状态目录路径不得包含父目录组件"));
            };
            let final_component = parts.peek().is_none();
            current = if final_component && create {
                current.private_child(name)?
            } else {
                current.open_child(name, final_component)?
            };
        }
        Ok(current)
    }

    pub(super) fn private_child(&self, name: impl AsRef<OsStr>) -> Result<Self, StateError> {
        let name = name.as_ref();
        let dir = match self.open_child(name, true) {
            Ok(dir) => dir,
            Err(StateError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                match mkdirat(&self.fd, name, Mode::from_bits_truncate(0o700)) {
                    Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                    Err(err) => {
                        return Err(StateError::io(
                            "创建目录",
                            &self.path.join(name),
                            err.into(),
                        ));
                    }
                }
                self.open_child(name, true)?
            }
            Err(err) => return Err(err),
        };
        self.sync()?;
        Ok(dir)
    }

    fn open_child(&self, name: &OsStr, private: bool) -> Result<Self, StateError> {
        let path = self.path.join(name);
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let fd = File::from(
            openat(&self.fd, name, flags, Mode::empty())
                .map_err(|err| StateError::io("打开目录", &path, err.into()))?,
        );
        let meta = fd
            .metadata()
            .map_err(|err| StateError::io("检查目录", &path, err))?;
        let uid = geteuid().as_raw();
        let mode = meta.mode() & 0o7777;
        // A sticky directory such as /tmp permits creation but protects our entries.
        let trusted_ancestor = meta.uid() == 0 || meta.uid() == uid;
        let protected = mode & 0o022 == 0 || mode & 0o1000 != 0;
        if (private && (meta.uid() != uid || mode != 0o700))
            || (!private && (!trusted_ancestor || !protected))
        {
            return Err(StateError::invalid(
                &path,
                "目录属主或权限不符合目标状态目录要求",
            ));
        }
        Ok(Self { path, fd })
    }

    pub(super) fn read(&self, name: &str) -> Result<Option<Vec<u8>>, StateError> {
        let flags = OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let path = self.path.join(name);
        let fd = match openat(&self.fd, name, flags, Mode::empty()) {
            Ok(fd) => File::from(fd),
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(err) => return Err(StateError::io("读取文件", &path, err.into())),
        };
        validate_file(&fd, &path)?;
        let mut data = Vec::new();
        (&fd)
            .read_to_end(&mut data)
            .map_err(|err| StateError::io("读取文件", &path, err))?;
        Ok(Some(data))
    }

    pub(super) fn lock(&self, name: &str) -> Result<File, StateError> {
        let flags =
            OFlags::RDWR | OFlags::CREATE | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let path = self.path.join(name);
        let fd = File::from(
            openat(&self.fd, name, flags, Mode::from_bits_truncate(0o600))
                .map_err(|err| StateError::io("打开状态锁", &path, err.into()))?,
        );
        validate_file(&fd, &path)?;
        fd.lock()
            .map_err(|err| StateError::io("获取状态锁", &path, err))?;
        Ok(fd)
    }

    pub(super) fn file_names(&self) -> Result<Vec<String>, StateError> {
        let error = |err: rustix::io::Errno| StateError::io("列出目录", &self.path, err.into());
        let mut files = Vec::new();
        for entry in Dir::read_from(&self.fd).map_err(error)? {
            let entry = entry.map_err(error)?;
            if let Ok(name) = entry.file_name().to_str() {
                files.push(name.into());
            }
        }
        Ok(files)
    }

    pub(super) fn publish(&self, name: &str, data: &[u8]) -> Result<(), StateError> {
        self.write_atomically(name, data, RenameFlags::NOREPLACE)
    }

    pub(super) fn replace(&self, name: &str, data: &[u8]) -> Result<(), StateError> {
        self.write_atomically(name, data, RenameFlags::empty())
    }

    fn write_atomically(
        &self,
        name: &str,
        data: &[u8],
        rename_flags: RenameFlags,
    ) -> Result<(), StateError> {
        static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
        let path = self.path.join(name);
        let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC;
        let mode = Mode::from_bits_truncate(0o600);
        let (temp, mut file) = loop {
            let count = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let temp = format!(".stage-{}-{count}", std::process::id());
            match openat(&self.fd, temp.as_str(), flags, mode) {
                Ok(fd) => break (temp, File::from(fd)),
                Err(rustix::io::Errno::EXIST) => continue,
                Err(err) => return Err(StateError::io("暂存文件", &path, err.into())),
            }
        };
        // Set the exact persisted mode even when umask masks owner permissions.
        let result = fchmod(&file, mode)
            .map_err(io::Error::from)
            .and_then(|()| file.write_all(data))
            .and_then(|()| file.sync_all())
            .and_then(|()| {
                renameat_with(&self.fd, temp.as_str(), &self.fd, name, rename_flags)
                    .map_err(io::Error::from)
            });
        if let Err(err) = result {
            let _ = unlinkat(&self.fd, temp.as_str(), rustix::fs::AtFlags::empty());
            return Err(StateError::io("发布文件", &path, err));
        }
        self.sync()
    }

    pub(super) fn remove(&self, name: &str) -> Result<(), StateError> {
        unlinkat(&self.fd, name, rustix::fs::AtFlags::empty())
            .map_err(|err| StateError::io("移除已回收的文件", &self.path.join(name), err.into()))
    }

    pub(super) fn sync(&self) -> Result<(), StateError> {
        self.fd
            .sync_all()
            .map_err(|err| StateError::io("同步目录", &self.path, err))
    }
}

fn optional<T>(result: Result<T, StateError>) -> Result<Option<T>, StateError> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(StateError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

fn validate_file(file: &File, path: &Path) -> Result<(), StateError> {
    let meta = file
        .metadata()
        .map_err(|err| StateError::io("检查文件", path, err))?;
    if !meta.is_file()
        || meta.uid() != geteuid().as_raw()
        || meta.nlink() != 1
        || meta.mode() & 0o7777 != 0o600
    {
        return Err(StateError::invalid(
            path,
            "状态文件需要当前目标端账户所有、权限 0600、无额外硬链接的普通文件",
        ));
    }
    Ok(())
}
