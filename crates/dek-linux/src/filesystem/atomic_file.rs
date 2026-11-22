use std::{
    ffi::OsString,
    fs::File,
    io::{self, Seek, Write},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

use rustix::{
    fd::OwnedFd,
    fs::{
        AtFlags, FileType, Mode, OFlags, Stat, fchmod, fchown, fstat, mkdirat, open, openat,
        renameat, statat, unlinkat,
    },
    io::Errno,
    process::{Gid, Uid, geteuid},
};

pub(crate) struct TargetPath {
    parent: OwnedFd,
    name: OsString,
}

impl TargetPath {
    /// Open the parent without following links. None means an ancestor is absent.
    pub(crate) fn open(path: &Path) -> io::Result<Option<Self>> {
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected an absolute path",
            ));
        }
        let name = path
            .file_name()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "expected a target file name")
            })?
            .to_owned();
        let directory = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let mut parent = open("/", directory, Mode::empty())?;
        if let Some(prefix) = path.parent() {
            for part in prefix.iter().skip(1) {
                parent = match openat(&parent, part, directory, Mode::empty()) {
                    Ok(fd) => fd,
                    Err(Errno::NOENT) => return Ok(None),
                    Err(err) => return Err(err.into()),
                };
            }
        }
        Ok(Some(Self { parent, name }))
    }

    pub(crate) fn ensure_directory(&self, mode: Mode) -> io::Result<()> {
        match mkdirat(&self.parent, &self.name, mode) {
            Ok(()) => rustix::fs::fsync(&self.parent)?,
            Err(Errno::EXIST) => {}
            Err(err) => return Err(err.into()),
        }
        openat(
            &self.parent,
            &self.name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        Ok(())
    }

    pub(crate) fn observe(&self) -> io::Result<Option<Stat>> {
        match statat(&self.parent, &self.name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile => {
                Ok(Some(stat))
            }
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected a regular file; symbolic links are not followed",
            )),
            Err(Errno::NOENT) => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    pub(crate) fn read(&self, expected: &Stat) -> io::Result<File> {
        let file = File::from(openat(
            &self.parent,
            &self.name,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        if !same_revision(expected, &fstat(&file)?) {
            return Err(changed_path());
        }
        Ok(file)
    }

    pub(crate) fn require_unchanged(&self, expected: Option<&Stat>) -> io::Result<()> {
        let actual = self.observe()?;
        match (expected, actual.as_ref()) {
            (None, None) => Ok(()),
            (Some(expected), Some(actual)) if same_revision(expected, actual) => Ok(()),
            _ => Err(changed_path()),
        }
    }

    pub(crate) fn remove(&self, expected: &Stat) -> io::Result<()> {
        self.require_unchanged(Some(expected))?;
        unlinkat(&self.parent, &self.name, AtFlags::empty())?;
        rustix::fs::fsync(&self.parent)?;
        Ok(())
    }
}

pub(crate) fn same_revision(expected: &Stat, actual: &Stat) -> bool {
    same_inode(expected, actual)
        && expected.st_mtime == actual.st_mtime
        && expected.st_mtime_nsec == actual.st_mtime_nsec
        && expected.st_ctime == actual.st_ctime
        && expected.st_ctime_nsec == actual.st_ctime_nsec
        && expected.st_size == actual.st_size
}

fn same_inode(expected: &Stat, actual: &Stat) -> bool {
    expected.st_dev == actual.st_dev && expected.st_ino == actual.st_ino
}

fn changed_path() -> io::Error {
    io::Error::other("the target file changed while preparing the action")
}

pub(crate) struct StagedFile<'a> {
    target: &'a TargetPath,
    directory: OwnedFd,
    directory_name: String,
}

impl<'a> StagedFile<'a> {
    pub(crate) fn new(
        target: &'a TargetPath,
        bytes: &[u8],
        uid: Option<Uid>,
        gid: Option<Gid>,
        mode: Mode,
    ) -> io::Result<Self> {
        let stage = Self::private_directory(target)?;
        let mut file = File::from(openat(
            &stage.directory,
            "content",
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?);
        file.write_all(bytes)?;
        if uid.is_some() || gid.is_some() {
            fchown(&file, uid, gid)?;
        }
        fchmod(&file, mode)?;
        file.rewind()?;
        match super::hash_bytes(&mut file)? {
            digest if digest == super::digest(bytes) => {}
            _ => {
                return Err(io::Error::other(
                    "staged bytes did not match the reviewed content",
                ));
            }
        }
        file.sync_all()?;
        Ok(stage)
    }

    fn private_directory(target: &'a TargetPath) -> io::Result<Self> {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let mode = Mode::RUSR | Mode::WUSR | Mode::XUSR;
        let directory_name = loop {
            let number = SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let name = format!(".dek-stage-{}-{number}", std::process::id());
            match mkdirat(&target.parent, &name, mode) {
                Ok(()) => break name,
                Err(Errno::EXIST) => continue,
                Err(error) => return Err(error.into()),
            }
        };
        let directory = openat(
            &target.parent,
            &directory_name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let stat = fstat(&directory)?;
        if stat.st_uid != geteuid().as_raw() || stat.st_mode & 0o7777 != 0o700 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe staging directory",
            ));
        }
        Ok(Self {
            target,
            directory,
            directory_name,
        })
    }

    pub(crate) fn publish(self, expected: Option<&Stat>) -> io::Result<()> {
        self.target.require_unchanged(expected)?;
        renameat(
            &self.directory,
            "content",
            &self.target.parent,
            &self.target.name,
        )?;
        rustix::fs::fsync(&self.target.parent)?;
        Ok(())
    }
}

impl Drop for StagedFile<'_> {
    fn drop(&mut self) {
        let _ = unlinkat(&self.directory, "content", AtFlags::empty());
        let info = fstat(&self.directory).ok().zip(
            statat(
                &self.target.parent,
                &self.directory_name,
                AtFlags::SYMLINK_NOFOLLOW,
            )
            .ok(),
        );
        if info.is_some_and(|(opened, named)| same_inode(&opened, &named)) {
            let _ = unlinkat(
                &self.target.parent,
                &self.directory_name,
                AtFlags::REMOVEDIR,
            );
        }
    }
}
