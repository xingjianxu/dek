use std::{
    ffi::OsStr,
    fs::File,
    io::{self, Read, Write},
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

use dek_target::backend::BackendError;
use rustix::{
    fs::{
        AtFlags, Gid, Mode, OFlags, Uid, XattrFlags, fchmod, fchown, fgetxattr, fgetxattr_query,
        flistxattr, flistxattr_query, fsetxattr, linkat, open, openat, renameat, statat, unlinkat,
    },
    io::Errno,
};

unsafe extern "C" {
    fn lckpwdf() -> libc::c_int;
    fn ulckpwdf() -> libc::c_int;
}

const DB_LOCK: &str = "passwd.lock";

struct GlobalLock;

impl GlobalLock {
    fn acquire() -> io::Result<Self> {
        // libc takes the POSIX record lock used by Shadow on /etc/.pwd.lock.
        if unsafe { lckpwdf() } == 0 {
            Ok(Self)
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

impl Drop for GlobalLock {
    fn drop(&mut self) {
        // This guard owns the libc lock for the executing process.
        unsafe { ulckpwdf() };
    }
}

struct Stage<'a> {
    dir: &'a File,
    name: Option<String>,
    file: File,
}

impl<'a> Stage<'a> {
    fn create(dir: &'a File) -> io::Result<Self> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC;
        loop {
            let name = format!(
                ".dek-passwd-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            );
            match openat(dir, name.as_str(), flags, Mode::RUSR | Mode::WUSR) {
                Ok(fd) => {
                    return Ok(Self {
                        dir,
                        name: Some(name),
                        file: File::from(fd),
                    });
                }
                Err(Errno::EXIST) => continue,
                Err(err) => return Err(err.into()),
            }
        }
    }
}

impl Drop for Stage<'_> {
    fn drop(&mut self) {
        if let Some(name) = &self.name {
            let _ = unlinkat(self.dir, name.as_str(), AtFlags::empty());
        }
    }
}

struct PasswdLock<'a> {
    dir: &'a File,
    device: u64,
    inode: u64,
}

impl<'a> PasswdLock<'a> {
    fn acquire(dir: &'a File) -> io::Result<Self> {
        let mut stage = Stage::create(dir)?;
        fchmod(&stage.file, Mode::RUSR | Mode::WUSR)?;
        write!(stage.file, "{}\0", std::process::id())?;
        stage.file.sync_all()?;
        let metadata = stage.file.metadata()?;
        // Shadow publishes this PID format with a hard link that cannot replace a lock.
        linkat(
            dir,
            stage.name.as_deref().unwrap(),
            dir,
            DB_LOCK,
            AtFlags::empty(),
        )?;
        Ok(Self {
            dir,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

impl Drop for PasswdLock<'_> {
    fn drop(&mut self) {
        if statat(self.dir, DB_LOCK, AtFlags::SYMLINK_NOFOLLOW)
            .is_ok_and(|actual| actual.st_dev == self.device && actual.st_ino == self.inode)
        {
            let _ = unlinkat(self.dir, DB_LOCK, AtFlags::empty());
        }
    }
}

pub(super) fn change(
    etc: &Path,
    user: &str,
    uid: u32,
    old_gid: u32,
    resolve: impl FnOnce() -> Result<u32, BackendError>,
) -> Result<(), BackendError> {
    // The global libc lock addresses the active system database only.
    if etc != Path::new("/etc") {
        return Err(BackendError("主组变更需要目标机本地账户数据库".into()));
    }
    change_local(user, uid, old_gid, resolve)
        .map_err(|err| BackendError(format!("设置用户 {user:?} 的主组：{err}")))
}

fn change_local(
    user: &str,
    uid: u32,
    old_gid: u32,
    resolve: impl FnOnce() -> Result<u32, BackendError>,
) -> io::Result<()> {
    let dir = File::from(open(
        "/etc",
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    let directory = dir.metadata()?;
    if directory.uid() != 0 || directory.mode() & 0o022 != 0 {
        return Err(io::Error::other("/etc 目录权限不安全"));
    }
    let _global = GlobalLock::acquire()?;
    let _local = PasswdLock::acquire(&dir)?;
    let new_gid = resolve().map_err(io::Error::other)?;
    let mut source = File::from(openat(
        &dir,
        "passwd",
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    let old = source.metadata()?;
    if !old.is_file() || old.uid() != 0 || old.mode() & 0o022 != 0 {
        return Err(io::Error::other("passwd 文件类型或权限不安全"));
    }
    let mut previous = Vec::new();
    source.read_to_end(&mut previous)?;
    let next = replace_gid(&previous, user, uid, old_gid, new_gid)?;
    let mut stage = Stage::create(&dir)?;
    stage.file.write_all(&next)?;
    fchown(
        &stage.file,
        Some(Uid::from_raw_unchecked(old.uid())),
        Some(Gid::from_raw_unchecked(old.gid())),
    )?;
    fchmod(&stage.file, Mode::from_bits_truncate(old.mode()))?;
    copy_xattrs(&source, &stage.file)?;
    stage.file.sync_all()?;
    let current = statat(&dir, "passwd", AtFlags::SYMLINK_NOFOLLOW)?;
    if current.st_dev != old.dev()
        || current.st_ino != old.ino()
        || current.st_size != old.size() as i64
        || current.st_ctime != old.ctime()
        || current.st_ctime_nsec as i64 != old.ctime_nsec()
    {
        return Err(io::Error::other("passwd 文件在提交前已改变，请重新规划"));
    }
    renameat(&dir, stage.name.as_deref().unwrap(), &dir, "passwd")?;
    stage.name = None;
    dir.sync_all()
}

fn copy_xattrs(source: &File, stage: &File) -> io::Result<()> {
    let size = match flistxattr_query(source) {
        Ok(size) => size,
        Err(Errno::NOTSUP) => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    let mut names = vec![0u8; size];
    let size = flistxattr(source, names.as_mut_slice())?;
    for name in names[..size]
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
    {
        let name = OsStr::from_bytes(name);
        let mut value = vec![0u8; fgetxattr_query(source, name)?];
        let size = fgetxattr(source, name, value.as_mut_slice())?;
        fsetxattr(stage, name, &value[..size], XattrFlags::empty())?;
    }
    Ok(())
}

pub(super) fn replace_gid(
    previous: &[u8],
    user: &str,
    uid: u32,
    old_gid: u32,
    new_gid: u32,
) -> io::Result<Vec<u8>> {
    let mut next = Vec::with_capacity(previous.len());
    let mut found = false;
    for line in previous.split_inclusive(|byte| *byte == b'\n') {
        let fields: Vec<_> = line.split(|byte| *byte == b':').collect();
        if fields.first().copied() != Some(user.as_bytes()) {
            next.extend_from_slice(line);
            continue;
        }
        let [
            name,
            credential,
            current_uid,
            current_gid,
            description,
            home,
            shell,
        ] = fields.as_slice()
        else {
            return Err(io::Error::other("passwd 用户记录无效"));
        };
        let numeric = |bytes: &[u8]| {
            std::str::from_utf8(bytes)
                .ok()
                .and_then(|text| text.parse().ok())
        };
        if found || numeric(current_uid) != Some(uid) || numeric(current_gid) != Some(old_gid) {
            return Err(io::Error::other(
                "passwd 用户身份与审阅前置条件不符，请重新规划",
            ));
        }
        found = true;
        let gid = new_gid.to_string();
        let fields = [
            *name,
            *credential,
            *current_uid,
            gid.as_bytes(),
            *description,
            *home,
            *shell,
        ];
        next.extend(fields.join(b":".as_slice()));
    }
    if !found {
        return Err(io::Error::other("passwd 用户已不存在，请重新规划"));
    }
    Ok(next)
}
