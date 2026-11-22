//! Prepare native ALPM plans and execute reviewed repository transactions.

mod cache;
mod config;
mod execution;
mod native;
mod upgrade_apply;

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::CString,
    fs, io,
    os::unix::{ffi::OsStrExt, fs::PermissionsExt},
    path::Path,
    ptr::{self, NonNull},
};

use dek_core::{
    RepositoryPackageCandidate, RepositoryPackageChange, RepositoryTransaction,
    RepositoryTransactionScope,
};
use dek_target::backend::BackendError;

use config::{Config, INSTALL, Repository, SYNC, UPGRADE};
use native::{Database, Handle, List, Native, Package, Question, ReplaceQuestion, items, text};

pub(super) use execution::prepare_transaction;

pub(super) fn preview(
    installed: &BTreeMap<&str, &str>,
    targets: Vec<String>,
    upgrade: bool,
) -> Result<(RepositoryTransaction, String), BackendError> {
    let config = Config::read()?;
    let db = tempfile::tempdir_in("/tmp").map_err(db_error)?;
    let path = db.path();
    let source_local = config.db.join("local");
    let lock = config.db.join("db.lck");
    let old_modified = fs::metadata(&source_local)
        .and_then(|info| info.modified())
        .map_err(db_error)?;
    if lock.try_exists().map_err(db_error)? {
        return Err(BackendError(
            "系统 pacman 数据库正被占用，请在包事务结束后重新规划".into(),
        ));
    }
    copy_local(&source_local, &path.join("local")).map_err(db_error)?;
    let metadata_changed = fs::metadata(&source_local)
        .and_then(|info| info.modified())
        .map_err(db_error)?
        != old_modified;
    if metadata_changed || lock.try_exists().map_err(db_error)? {
        return Err(BackendError(
            "读取期间系统 pacman 数据库已变更，请重新规划".into(),
        ));
    }
    let sync = path.join("sync");
    fs::create_dir(&sync).map_err(db_error)?;
    for repo in &config.repositories {
        for suffix in [".db", ".db.sig"] {
            let file = format!("{}{suffix}", repo.name);
            copy_if_exists(&config.db.join("sync").join(&file), &sync.join(file))
                .map_err(db_error)?;
        }
    }
    let gpg = path.join("gnupg");
    snapshot_public_keyring(&config.gpg, &gpg).map_err(db_error)?;
    if unsafe { libc::geteuid() } == 0 && config.download_user.is_some() {
        // The downloader needs to traverse the scratch directory. The keyring
        // remains private and ALPM owns the temporary download directory.
        fs::set_permissions(path, fs::Permissions::from_mode(0o711)).map_err(db_error)?;
    }
    let alpm = Alpm::open(&config, path, &gpg, &path.join("pacman.log"))?;
    let local = alpm.local_versions()?;
    if local.len() != installed.len()
        || local
            .iter()
            .any(|(name, version)| installed.get(name.as_str()) != Some(&version.as_str()))
    {
        return Err(BackendError(
            "本机包版本与 pacman 数据库副本不一致，请重新规划".into(),
        ));
    }
    let reason = if upgrade {
        "已请求完整系统升级预览".into()
    } else {
        match alpm.cached_install(&targets) {
            Ok(transaction) => return Ok((transaction, "已用本机同步数据库确认可独立补装".into())),
            Err(reason) => reason,
        }
    };
    alpm.refresh(&config, &sync)?;
    let transaction = alpm.resolve(RepositoryTransactionScope::Upgrade, targets)?;
    Ok((transaction, reason))
}

fn db_error(error: io::Error) -> BackendError {
    BackendError(format!("无法准备隔离的 pacman 数据库：{error}"))
}

fn copy_local(source: &Path, target: &Path) -> io::Result<()> {
    fs::create_dir(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let path = target.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_local(&entry.path(), &path)?;
        } else if entry.file_type()?.is_file() {
            fs::copy(entry.path(), path)?;
        } else {
            return Err(io::Error::other(format!(
                "不支持的 pacman 本地数据库条目 {}",
                entry.path().display()
            )));
        }
    }
    Ok(())
}

fn copy_if_exists(source: &Path, target: &Path) -> io::Result<()> {
    match fs::copy(source, target) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn ignore_missing_or_permissions(copied: io::Result<u64>) -> io::Result<()> {
    match copied {
        Ok(_) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn snapshot_public_keyring(source: &Path, staging: &Path) -> io::Result<()> {
    fs::create_dir(staging)?;
    fs::set_permissions(staging, fs::Permissions::from_mode(0o700))?;
    for file in ["pubring.gpg", "pubring.kbx", "trustdb.gpg", "gpg.conf"] {
        // ALPM rejects signed metadata if the snapshot lacks a trusted key.
        ignore_missing_or_permissions(fs::copy(source.join(file), staging.join(file)))?;
    }
    Ok(())
}

struct Alpm {
    native: Native,
    handle: NonNull<Handle>,
    repositories: Vec<(Repository, NonNull<Database>)>,
    sync: std::path::PathBuf,
}

impl Drop for Alpm {
    fn drop(&mut self) {
        // SAFETY: the native handle is initialized and exclusively owned here.
        unsafe { (self.native.alpm_release)(self.handle.as_ptr()) };
    }
}

fn as_c(value: impl AsRef<[u8]>) -> Result<CString, BackendError> {
    CString::new(value.as_ref()).map_err(|_| BackendError("pacman 参数含有 NUL 字节".into()))
}

impl Alpm {
    fn open(
        config: &Config,
        database: &Path,
        gpg: &Path,
        log: &Path,
    ) -> Result<Self, BackendError> {
        let mut alpm = Self::initialize(config, database)?;
        alpm.configure(config, gpg, log)?;
        Ok(alpm)
    }

    fn initialize(config: &Config, database: &Path) -> Result<Self, BackendError> {
        let native = Native::load()?;
        let root = as_c(config.root.as_os_str().as_bytes())?;
        let db = as_c(database.as_os_str().as_bytes())?;
        let mut error = 0;
        // SAFETY: all strings live through the call and ALPM copies the paths.
        let handle = unsafe { (native.alpm_initialize)(root.as_ptr(), db.as_ptr(), &mut error) };
        let handle = NonNull::new(handle).ok_or_else(|| {
            // SAFETY: ALPM returned this error code from initialization.
            let message = unsafe { text((native.alpm_strerror)(error)) }.unwrap_or_default();
            BackendError(format!("无法初始化 pacman 数据库：{message}"))
        })?;
        Ok(Self {
            native,
            handle,
            repositories: Vec::new(),
            sync: database.join("sync"),
        })
    }

    fn configure(&mut self, config: &Config, gpg: &Path, log: &Path) -> Result<(), BackendError> {
        let native = &self.native;
        let handle = self.handle.as_ptr();
        let gpg = as_c(gpg.as_os_str().as_bytes())?;
        let log = as_c(log.as_os_str().as_bytes())?;
        // SAFETY: the handle is live, and ALPM copies strings passed to its setters.
        unsafe {
            self.check(
                (native.alpm_option_set_gpgdir)(handle, gpg.as_ptr()),
                "设置密钥目录",
            )?;
            self.check(
                (native.alpm_option_set_logfile)(handle, log.as_ptr()),
                "设置日志路径",
            )?;
            self.check(
                (native.alpm_option_set_default_siglevel)(handle, config.signatures),
                "设置签名策略",
            )?;
            self.check(
                (native.alpm_option_set_disable_dl_timeout)(
                    handle,
                    u16::from(config.disable_timeout),
                ),
                "设置下载超时",
            )?;
            self.check(
                (native.alpm_option_set_disable_sandbox_filesystem)(
                    handle,
                    u16::from(config.disable_sandbox_filesystem),
                ),
                "设置下载文件系统隔离",
            )?;
            self.check(
                (native.alpm_option_set_disable_sandbox_syscalls)(
                    handle,
                    u16::from(config.disable_sandbox_syscalls),
                ),
                "设置下载进程隔离",
            )?;
            if libc::geteuid() == 0
                && let Some(user) = &config.download_user
            {
                let user = as_c(user)?;
                self.check(
                    (native.alpm_option_set_sandboxuser)(handle, user.as_ptr()),
                    "设置下载用户",
                )?;
            }
            for architecture in &config.architectures {
                let architecture = as_c(architecture)?;
                self.check(
                    (native.alpm_option_add_architecture)(handle, architecture.as_ptr()),
                    "设置包架构",
                )?;
            }
            for name in &config.ignored_packages {
                let name = as_c(name)?;
                self.check(
                    (native.alpm_option_add_ignorepkg)(handle, name.as_ptr()),
                    "设置忽略软件包",
                )?;
            }
            for group in &config.ignored_groups {
                let group = as_c(group)?;
                self.check(
                    (native.alpm_option_add_ignoregroup)(handle, group.as_ptr()),
                    "设置忽略软件包组",
                )?;
            }
            for repo in &config.repositories {
                let name = as_c(&repo.name)?;
                let database =
                    (native.alpm_register_syncdb)(handle, name.as_ptr(), repo.signatures);
                let database = NonNull::new(database)
                    .ok_or_else(|| self.error(&format!("注册仓库 {}", repo.name)))?;
                self.check(
                    (native.alpm_db_set_usage)(database.as_ptr(), repo.usage),
                    &format!("设置仓库 {} 的用途", repo.name),
                )?;
                for server in &repo.servers {
                    let server = as_c(server)?;
                    self.check(
                        (native.alpm_db_add_server)(database.as_ptr(), server.as_ptr()),
                        &format!("设置仓库 {} 的镜像", repo.name),
                    )?;
                }
                for server in &repo.cache_servers {
                    let server = as_c(server)?;
                    self.check(
                        (native.alpm_db_add_cache_server)(database.as_ptr(), server.as_ptr()),
                        &format!("设置仓库 {} 的缓存源", repo.name),
                    )?;
                }
                self.repositories.push((repo.clone(), database));
            }
        }
        Ok(())
    }

    fn cached_install(&self, targets: &[String]) -> Result<RepositoryTransaction, String> {
        if let Some(reason) = self
            .repositories
            .iter()
            .find_map(|(repo, db)| self.validate_repository(repo, *db).err())
        {
            return Err(format!("需要刷新仓库元数据。{reason}"));
        }
        if targets
            .iter()
            .any(|name| self.find_package(name, INSTALL).is_none())
        {
            return Err(format!(
                "需要刷新仓库元数据以查找候选包 {}",
                targets.join(", ")
            ));
        }
        let uncertain = |err: BackendError| format!("无法确认补装与本机一致。{err}");
        let upgrade = self
            .resolve(RepositoryTransactionScope::Upgrade, Vec::new())
            .map_err(uncertain)?;
        if !upgrade.changes.is_empty() {
            return Err("本机同步数据库已有待升级的软件包".into());
        }
        if let Some((name, version)) = self.local_ahead().map_err(uncertain)? {
            return Err(format!(
                "无法确认补装与本机一致。已安装的 {name} 比本机同步数据库中的 {version} 更新"
            ));
        }
        let install = self
            .resolve(RepositoryTransactionScope::Install, targets.to_vec())
            .map_err(uncertain)?;
        if install
            .changes
            .iter()
            .any(|change| !matches!(change, RepositoryPackageChange::Install { .. }))
        {
            return Err("无法确认补装与本机一致。补装会改变已有软件包".into());
        }
        Ok(install)
    }

    fn local_ahead(&self) -> Result<Option<(String, String)>, BackendError> {
        for (name, installed) in self.local_versions()? {
            if let Some(pkg) = self.find_package(&name, UPGRADE) {
                let version = self.package_version(pkg)?;
                let installed = as_c(&installed)?;
                let candidate = as_c(&version)?;
                // SAFETY: both versions are NUL-terminated strings.
                if unsafe { (self.native.alpm_pkg_vercmp)(installed.as_ptr(), candidate.as_ptr()) }
                    > 0
                {
                    return Ok(Some((name, version)));
                }
            }
        }
        Ok(None)
    }

    fn refresh(&self, config: &Config, sync: &Path) -> Result<(), BackendError> {
        if config.external_download
            && config
                .repositories
                .iter()
                .any(|repo| repo.usage & SYNC != 0)
        {
            return Err(BackendError(
                "需要刷新仓库元数据。隔离预览暂不支持 pacman 配置的 XferCommand".into(),
            ));
        }
        for (repo, db) in &self.repositories {
            if repo.usage & SYNC != 0 {
                // A new download must succeed; cached or partly refreshed data
                // must never be mistaken for a fresh repository transaction.
                for suffix in [".db", ".db.sig"] {
                    let file = sync.join(format!("{}{suffix}", repo.name));
                    match fs::remove_file(file) {
                        Ok(()) => {}
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => return Err(db_error(error)),
                    }
                }
                let mut list = List {
                    data: db.as_ptr().cast(),
                    prev: ptr::null_mut(),
                    next: ptr::null_mut(),
                };
                list.prev = &mut list;
                // SAFETY: the DB and the single-node list are live through the call.
                let result =
                    unsafe { (self.native.alpm_db_update)(self.handle.as_ptr(), &mut list, 1) };
                self.check(
                    result,
                    &format!("下载或验签仓库 {} 的同步数据库", repo.name),
                )?;
            }
            self.validate_repository(repo, *db)?;
        }
        Ok(())
    }

    fn validate_repository(
        &self,
        repo: &Repository,
        database: NonNull<Database>,
    ) -> Result<(), BackendError> {
        if repo.usage & (INSTALL | UPGRADE | SYNC) == 0 {
            return Ok(());
        }
        let operation = format!("读取仓库 {} 的同步数据库", repo.name);
        let metadata = self.sync.join(format!("{}.db", repo.name));
        if !metadata.is_file() {
            return Err(BackendError(format!(
                "缺少仓库 {} 的同步数据库元数据",
                repo.name
            )));
        }
        // SAFETY: the database is registered with this ALPM handle.
        unsafe {
            self.check(
                (self.native.alpm_db_get_valid)(database.as_ptr()),
                &operation,
            )?;
            let cache = (self.native.alpm_db_get_pkgcache)(database.as_ptr());
            if cache.is_null() && (self.native.alpm_errno)(self.handle.as_ptr()) != 0 {
                return Err(self.error(&operation));
            }
        }
        Ok(())
    }

    fn local_versions(&self) -> Result<BTreeMap<String, String>, BackendError> {
        // SAFETY: ALPM owns the database, package cache and package objects.
        let packages = unsafe {
            (self.native.alpm_db_get_pkgcache)((self.native.alpm_get_localdb)(self.handle.as_ptr()))
        };
        if packages.is_null() && unsafe { (self.native.alpm_errno)(self.handle.as_ptr()) } != 0 {
            return Err(self.error("读取本地包数据库副本"));
        }
        unsafe { items::<Package>(packages) }
            .map(|package| Ok((self.package_name(package)?, self.package_version(package)?)))
            .collect()
    }

    fn find_package(&self, name: &str, usage: i32) -> Option<*mut Package> {
        let name = as_c(name).ok()?;
        self.repositories
            .iter()
            .filter(|(repo, _)| repo.usage & usage != 0)
            .find_map(|(_, db)| {
                // SAFETY: database and name remain valid through the call.
                let package = unsafe { (self.native.alpm_db_get_pkg)(db.as_ptr(), name.as_ptr()) };
                NonNull::new(package).map(NonNull::as_ptr)
            })
    }

    fn package_name(&self, package: *mut Package) -> Result<String, BackendError> {
        unsafe { text((self.native.alpm_pkg_get_name)(package)) }
            .ok_or_else(|| self.error("读取软件包名称"))
    }

    fn package_version(&self, package: *mut Package) -> Result<String, BackendError> {
        unsafe { text((self.native.alpm_pkg_get_version)(package)) }
            .ok_or_else(|| self.error("读取软件包版本"))
    }

    fn candidate(&self, package: *mut Package) -> Result<RepositoryPackageCandidate, BackendError> {
        let name = self.package_name(package)?;
        let version = self.package_version(package)?;
        let database = unsafe { (self.native.alpm_pkg_get_db)(package) };
        let repository = unsafe { text((self.native.alpm_db_get_name)(database)) }
            .ok_or_else(|| self.error("读取候选包仓库"))?;
        let sha256 = unsafe { text((self.native.alpm_pkg_get_sha256sum)(package)) }
            .filter(|value| value.len() == 64 && value.bytes().all(|c| c.is_ascii_hexdigit()))
            .ok_or_else(|| {
                BackendError(format!(
                    "缺少仓库 {repository} 候选包 {name} {version} 的 SHA-256"
                ))
            })?;
        Ok(RepositoryPackageCandidate {
            name,
            version,
            repository,
            sha256,
        })
    }

    fn resolve(
        &self,
        scope: RepositoryTransactionScope,
        targets: Vec<String>,
    ) -> Result<RepositoryTransaction, BackendError> {
        const NEEDED: i32 = 1 << 13;
        let native = &self.native;
        let handle = self.handle.as_ptr();
        self.check(
            unsafe { (native.alpm_trans_init)(handle, NEEDED) },
            "初始化隔离软件包事务",
        )?;
        let mut decisions = Vec::<(*mut Package, *mut Package)>::new();
        let result = self
            .check(
                unsafe {
                    (native.alpm_option_set_questioncb)(
                        handle,
                        Some(question),
                        ptr::from_mut(&mut decisions).cast(),
                    )
                },
                "设置预览事务决策",
            )
            .and_then(|()| self.prepare(scope, &targets))
            .and_then(|()| self.transaction_changes(&decisions));
        let released = self.check(
            unsafe { (native.alpm_trans_release)(handle) },
            "释放隔离软件包事务",
        );
        let reset = self.check(
            unsafe { (native.alpm_option_set_questioncb)(handle, None, ptr::null_mut()) },
            "清理隔离事务决策",
        );
        let changes = result?;
        released?;
        reset?;
        Ok(RepositoryTransaction {
            scope,
            targets,
            changes,
        })
    }

    fn prepare(
        &self,
        scope: RepositoryTransactionScope,
        targets: &[String],
    ) -> Result<(), BackendError> {
        let handle = self.handle.as_ptr();
        let native = &self.native;
        let mut missing = Vec::new();
        for name in targets {
            let Some(package) = self.find_package(name, INSTALL) else {
                missing.push(name.as_str());
                continue;
            };
            if unsafe { (native.alpm_pkg_should_ignore)(handle, package) } != 0 {
                return Err(BackendError(format!(
                    "pacman 配置忽略了声明的软件包 {name}"
                )));
            }
            self.check(
                unsafe { (native.alpm_add_pkg)(handle, package) },
                &format!("加入仓库软件包 {name}"),
            )?;
        }
        if !missing.is_empty() {
            return Err(BackendError(format!(
                "仓库同步数据库缺少软件包候选：{}",
                missing.join(", ")
            )));
        }
        if scope == RepositoryTransactionScope::Upgrade {
            self.check(
                unsafe { (native.alpm_sync_sysupgrade)(handle, 0) },
                "计算完整系统升级",
            )?;
        }
        let mut details = ptr::null_mut();
        let result = unsafe { (native.alpm_trans_prepare)(handle, &mut details) };
        if result < 0 {
            let code = unsafe { (native.alpm_errno)(handle) };
            let base = self.error("解析软件包事务");
            let details = self.prepare_error(code, details);
            return Err(BackendError(format!("{}{details}", base.0)));
        }
        Ok(())
    }

    fn prepare_error(&self, code: i32, details: *mut List) -> String {
        use native::{DependencyConflict, MissingDependency};
        // SAFETY: ALPM 16 documents the payload types for prepare errors. It
        // transfers ownership of both the detail entries and list to us.
        let messages = unsafe {
            let messages: Vec<String> = match code {
                42 => items::<libc::c_char>(details)
                    .filter_map(|entry| {
                        let message = text(entry);
                        libc::free(entry.cast());
                        message
                    })
                    .collect(),
                45 => items::<MissingDependency>(details)
                    .map(|entry| {
                        let target = text((*entry).target).unwrap_or_default();
                        let depend = (self.native.alpm_dep_compute_string)((*entry).depend);
                        let requirement = text(depend).unwrap_or_default();
                        libc::free(depend.cast());
                        (self.native.alpm_depmissing_free)(entry);
                        format!("{target} 需要 {requirement}")
                    })
                    .collect(),
                46 => items::<DependencyConflict>(details)
                    .map(|entry| {
                        let first = self.package_name((*entry).first).unwrap_or_default();
                        let second = self.package_name((*entry).second).unwrap_or_default();
                        (self.native.alpm_conflict_free)(entry);
                        format!("{first} 与 {second} 冲突")
                    })
                    .collect(),
                _ => Vec::new(),
            };
            (self.native.alpm_list_free)(details);
            messages
        };
        if messages.is_empty() {
            String::new()
        } else {
            format!("；{}", messages.join("；"))
        }
    }

    fn transaction_changes(
        &self,
        decisions: &[(*mut Package, *mut Package)],
    ) -> Result<Vec<RepositoryPackageChange>, BackendError> {
        let handle = self.handle.as_ptr();
        let native = &self.native;
        let local = self.local_versions()?;
        let mut removed = unsafe { items::<Package>((native.alpm_trans_get_remove)(handle)) }
            .map(|package| Ok((self.package_name(package)?, self.package_version(package)?)))
            .collect::<Result<BTreeMap<_, _>, BackendError>>()?;
        let mut replacements = BTreeMap::<String, BTreeSet<String>>::new();
        for &(old, new) in decisions {
            replacements
                .entry(self.package_name(new)?)
                .or_default()
                .insert(self.package_name(old)?);
        }
        let mut changes = Vec::new();
        for package in unsafe { items::<Package>((native.alpm_trans_get_add)(handle)) } {
            let candidate = self.candidate(package)?;
            let mut replaces = false;
            for name in replacements.remove(&candidate.name).into_iter().flatten() {
                if let Some(installed_version) = removed.remove(&name) {
                    changes.push(RepositoryPackageChange::Replace {
                        package: name,
                        installed_version,
                        candidate: candidate.clone(),
                    });
                    replaces = true;
                }
            }
            if let Some(installed_version) = local.get(&candidate.name) {
                changes.push(RepositoryPackageChange::Upgrade {
                    installed_version: installed_version.clone(),
                    candidate,
                });
            } else if !replaces {
                changes.push(RepositoryPackageChange::Install { candidate });
            }
        }
        for (package, installed_version) in removed {
            changes.push(RepositoryPackageChange::Remove {
                package,
                installed_version,
            });
        }
        Ok(changes)
    }

    fn check(&self, result: i32, operation: &str) -> Result<(), BackendError> {
        if result < 0 {
            Err(self.error(operation))
        } else {
            Ok(())
        }
    }

    fn error(&self, operation: &str) -> BackendError {
        let code = unsafe { (self.native.alpm_errno)(self.handle.as_ptr()) };
        let message = unsafe { text((self.native.alpm_strerror)(code)) }.unwrap_or_default();
        BackendError(format!("{operation}失败：{message}"))
    }
}

unsafe extern "C" fn question(context: *mut libc::c_void, question: *mut Question) {
    const REPLACE: i32 = 1 << 1;
    const REMOVE_CONFLICT: i32 = 1 << 2;
    // SAFETY: libalpm passes a live question and the context set by resolve.
    unsafe {
        match (*question).kind {
            REPLACE => {
                let replacement = &mut *question.cast::<ReplaceQuestion>();
                let decisions = &mut *context.cast::<Vec<(*mut Package, *mut Package)>>();
                decisions.push((replacement.old, replacement.new));
                replacement.question.answer = 1;
            }
            REMOVE_CONFLICT => (*question).answer = 1,
            // First provider, no ignored package override, and no dropping
            // unresolvable packages or importing unreviewed signing keys.
            _ => (*question).answer = 0,
        }
    }
}
