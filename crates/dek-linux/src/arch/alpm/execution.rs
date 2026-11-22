//! Bind archive selection and the native commit to a reviewed transaction.

use std::{
    fs, io,
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, PermissionsExt},
    },
    path::Path,
    ptr,
};

use base64::{Engine, engine::general_purpose::STANDARD};
use dek_core::{
    RepositoryPackageCandidate, RepositoryPackageChange, RepositoryTransaction,
    RepositoryTransactionScope,
};
use dek_target::backend::{BackendError, RepositoryPreparation};

use super::{
    Alpm, Config, Package, as_c, cache, items, native::FileConflict, question, text,
    upgrade_apply::UpgradeDatabase,
};

const NEEDED: i32 = 1 << 13;
const DOWNLOAD_ONLY: i32 = 1 << 9;
const SIGNED_PACKAGE: i32 = 1;

pub(in crate::arch) fn prepare_transaction(
    reviewed: &RepositoryTransaction,
) -> Result<RepositoryPreparation, BackendError> {
    if !rustix::process::geteuid().is_root() {
        return Err(BackendError("安装仓库软件包需要目标端 root 权限".into()));
    }
    let config = Config::read()?;
    let stage = tempfile::tempdir_in("/tmp").map_err(cache_error)?;
    // The native downloader owns a separate temporary directory inside this
    // directory. It can traverse the parent without modifying reviewed archives.
    fs::set_permissions(stage.path(), fs::Permissions::from_mode(0o711)).map_err(cache_error)?;

    let (live_lock, upgrade_db) = if reviewed.scope == RepositoryTransactionScope::Upgrade {
        let lock = Transaction::lock_live(&config)?;
        let snapshot = UpgradeDatabase::refresh(&config)?;
        (Some(lock), Some(snapshot))
    } else {
        (None, None)
    };
    let database = upgrade_db
        .as_ref()
        .map_or(config.db.as_path(), UpgradeDatabase::path);
    let private_keys = upgrade_db.as_ref().map(|db| db.path().join("gnupg"));
    let keys = private_keys.as_deref().unwrap_or(&config.gpg);
    let Some(fetch) = Transaction::begin(
        &config,
        database,
        keys,
        stage.path(),
        NEEDED | DOWNLOAD_ONLY,
    )?
    else {
        return Ok(RepositoryPreparation::Changed);
    };
    let Some(archives) = fetch.reviewed_archives(reviewed)? else {
        return Ok(RepositoryPreparation::Changed);
    };
    let mut missing = false;
    for archive in &archives {
        let cached = cache::stage_archive(
            &config.cache_dirs,
            stage.path(),
            &archive.filename,
            &archive.sha256,
        )
        .map_err(cache_error)?;
        let signature = stage.path().join(format!("{}.sig", archive.filename));
        if cached && let Some(embedded) = &archive.signature {
            fs::write(&signature, embedded).map_err(cache_error)?;
        }
        missing |= !cached || (archive.signed && !signature.is_file());
    }
    if !fetch.matches(reviewed)? {
        return Ok(RepositoryPreparation::Changed);
    }
    if missing && config.external_download {
        return Err(BackendError(
            "缺少已审阅的软件包缓存，目标配置的 XferCommand 下载暂不可用".into(),
        ));
    }
    if !archives.is_empty() {
        fetch.commit("下载并验签已审阅软件包")?;
    }
    verify_archives(stage.path(), &archives)?;
    drop(fetch);

    // Package files are now pinned in our private directory. Reacquire the
    // database lock, recheck the exact transaction, and hold it through commit.
    let Some(transaction) = Transaction::begin(&config, database, keys, stage.path(), NEEDED)?
    else {
        return Ok(RepositoryPreparation::Changed);
    };
    if transaction.reviewed_archives(reviewed)?.as_ref() != Some(&archives)
        || !transaction.matches(reviewed)?
    {
        return Ok(RepositoryPreparation::Changed);
    }
    let reviewed = reviewed.clone();
    Ok(RepositoryPreparation::Ready(Box::new(move || {
        let Some(database) = upgrade_db.as_ref() else {
            let _stage = stage;
            return transaction.commit("安装已审阅的仓库事务");
        };
        database.publish(&config.db)?;
        drop(transaction);
        // ALPM commit requires the lock to belong to the committing handle.
        // Reacquire it with the real DB path so archive scriptlets and metadata
        // are installed by the same rules as an ordinary pacman transaction.
        drop(live_lock);
        if reviewed.changes.is_empty() {
            return Ok(());
        }
        let keys = private_keys.as_deref().unwrap_or(&config.gpg);
        let live = Transaction::begin(&config, &config.db, keys, stage.path(), NEEDED)?
            .ok_or_else(|| BackendError("本机同步数据库在重新加锁时改变，请重新规划".into()))?;
        if live.reviewed_archives(&reviewed)?.as_ref() != Some(&archives)
            || !live.matches(&reviewed)?
        {
            return Err(BackendError(
                "仓库事务在重新加锁后与审阅清单不符，请重新规划".into(),
            ));
        }
        live.commit("执行已审阅的完整仓库升级")
    })))
}

#[derive(Eq, PartialEq)]
struct Archive {
    filename: String,
    sha256: String,
    signed: bool,
    signature: Option<Vec<u8>>,
}

fn verify_archives(stage: &Path, archives: &[Archive]) -> Result<(), BackendError> {
    for archive in archives {
        cache::pin_download(stage, &archive.filename).map_err(cache_error)?;
        cache::pin_download(stage, &format!("{}.sig", archive.filename)).map_err(cache_error)?;
        if !cache::matches_archive(&stage.join(&archive.filename), &archive.sha256)
            .map_err(cache_error)?
        {
            return Err(BackendError(format!(
                "软件包归档 {} 的 SHA-256 与审阅记录不符",
                archive.filename
            )));
        }
    }
    Ok(())
}

fn cache_error(error: io::Error) -> BackendError {
    BackendError(format!("无法准备已审阅的软件包缓存：{error}"))
}

struct Decisions(Vec<(*mut Package, *mut Package)>);

struct Transaction {
    alpm: Alpm,
    decisions: Box<Decisions>,
}

impl Drop for Transaction {
    fn drop(&mut self) {
        // SAFETY: this object owns the initialized transaction and callback.
        unsafe {
            let handle = self.alpm.handle.as_ptr();
            (self.alpm.native.alpm_trans_release)(handle);
            (self.alpm.native.alpm_option_set_questioncb)(handle, None, ptr::null_mut());
        }
    }
}

impl Transaction {
    fn lock_live(config: &Config) -> Result<Self, BackendError> {
        validate_local_database(&config.db)?;
        let alpm = Alpm::initialize(config, &config.db)?;
        Self::lock(alpm, 0)
    }

    fn lock(alpm: Alpm, flags: i32) -> Result<Self, BackendError> {
        let handle = alpm.handle.as_ptr();
        // SAFETY: ALPM owns the live handle and takes the selected database lock.
        alpm.check(
            unsafe { (alpm.native.alpm_trans_init)(handle, flags) },
            "锁定 pacman 数据库",
        )?;
        Ok(Self {
            alpm,
            decisions: Box::new(Decisions(Vec::new())),
        })
    }

    fn begin(
        config: &Config,
        database: &Path,
        keys: &Path,
        cache: &Path,
        flags: i32,
    ) -> Result<Option<Self>, BackendError> {
        validate_local_database(database)?;
        let databases = snapshot_sync(config, database)?;
        let alpm = Alpm::open(config, database, keys, &config.log)?;
        alpm.configure_execution(config, cache)?;
        let handle = alpm.handle.as_ptr();
        let mut transaction = Self::lock(alpm, flags)?;
        if snapshot_sync(config, database)? != databases {
            return Ok(None);
        }
        // SAFETY: callback state remains at a stable address until transaction release.
        transaction.alpm.check(
            unsafe {
                (transaction.alpm.native.alpm_option_set_questioncb)(
                    handle,
                    Some(question),
                    ptr::from_mut(&mut transaction.decisions.0).cast(),
                )
            },
            "设置审阅事务决策",
        )?;
        Ok(Some(transaction))
    }

    fn reviewed_archives(
        &self,
        reviewed: &RepositoryTransaction,
    ) -> Result<Option<Vec<Archive>>, BackendError> {
        let mut archives = Vec::new();
        for (repo, db) in &self.alpm.repositories {
            if self.alpm.validate_repository(repo, *db).is_err() {
                return Ok(None);
            }
        }
        for change in &reviewed.changes {
            let candidate = match change {
                RepositoryPackageChange::Install { candidate }
                | RepositoryPackageChange::Upgrade { candidate, .. }
                | RepositoryPackageChange::Replace { candidate, .. } => candidate,
                RepositoryPackageChange::Remove { .. } => continue,
            };
            let Some(archive) = self.archive(candidate)? else {
                return Ok(None);
            };
            if !archives.contains(&archive) {
                archives.push(archive);
            }
        }
        Ok(Some(archives))
    }

    fn archive(
        &self,
        candidate: &RepositoryPackageCandidate,
    ) -> Result<Option<Archive>, BackendError> {
        let Some((_, db)) = self
            .alpm
            .repositories
            .iter()
            .find(|(repo, _)| repo.name == candidate.repository)
        else {
            return Ok(None);
        };
        let name = as_c(&candidate.name)?;
        let native = &self.alpm.native;
        // SAFETY: the repository and package belong to the locked native handle.
        let package = unsafe { (native.alpm_db_get_pkg)(db.as_ptr(), name.as_ptr()) };
        if package.is_null() || self.alpm.candidate(package)? != *candidate {
            return Ok(None);
        }
        let filename = unsafe { text((native.alpm_pkg_get_filename)(package)) }
            .filter(|name| {
                Path::new(name)
                    .file_name()
                    .is_some_and(|filename| filename == name.as_str())
            })
            .ok_or_else(|| BackendError(format!("软件包 {} 缺少有效归档文件名", candidate.name)))?;
        let signed = unsafe { (native.alpm_db_get_siglevel)(db.as_ptr()) } & SIGNED_PACKAGE != 0;
        let signature = unsafe { text((native.alpm_pkg_get_base64_sig)(package)) }
            .map(|value| {
                STANDARD.decode(value).map_err(|error| {
                    BackendError(format!(
                        "无法读取软件包 {} 的仓库签名：{error}",
                        candidate.name
                    ))
                })
            })
            .transpose()?;
        Ok(Some(Archive {
            filename,
            sha256: candidate.sha256.clone(),
            signed,
            signature,
        }))
    }

    fn matches(&self, reviewed: &RepositoryTransaction) -> Result<bool, BackendError> {
        if reviewed.scope == RepositoryTransactionScope::Install
            && self.alpm.local_ahead()?.is_some()
        {
            return Ok(false);
        }
        // Including a system upgrade in preparation reveals any pending changes
        // to existing packages; the exact reviewed install scope still must match.
        let actual = match self
            .alpm
            .prepare(RepositoryTransactionScope::Upgrade, &reviewed.targets)
        {
            Ok(()) => self.alpm.transaction_changes(&self.decisions.0)?,
            Err(_) => return Ok(false),
        };
        Ok(actual == reviewed.changes)
    }

    fn commit(&self, operation: &str) -> Result<(), BackendError> {
        let native = &self.alpm.native;
        let handle = self.alpm.handle.as_ptr();
        let mut details = ptr::null_mut();
        // SAFETY: preparation checked the reviewed scope and this object keeps
        // the ALPM lock and stable callback state until commit returns.
        let result = unsafe { (native.alpm_trans_commit)(handle, &mut details) };
        if result == 0 {
            return Ok(());
        }
        let code = unsafe { (native.alpm_errno)(handle) };
        let error = self.alpm.error(operation);
        // SAFETY: ALPM 16 returns owned file conflicts or invalid archive names.
        let messages: Vec<_> = unsafe {
            let messages = match code {
                47 => items::<FileConflict>(details)
                    .map(|conflict| {
                        let file = text((*conflict).file).unwrap_or_default();
                        let package = text((*conflict).target).unwrap_or_default();
                        (native.alpm_fileconflict_free)(conflict);
                        format!("{package}：{file}")
                    })
                    .collect(),
                35..=38 => items::<libc::c_char>(details)
                    .filter_map(|value| {
                        let message = text(value);
                        libc::free(value.cast());
                        message
                    })
                    .collect(),
                _ => Vec::new(),
            };
            (native.alpm_list_free)(details);
            messages
        };
        if messages.is_empty() {
            Err(error)
        } else {
            Err(BackendError(format!(
                "{}。{}",
                error.0,
                messages.join("，")
            )))
        }
    }
}

#[derive(Eq, PartialEq)]
struct DatabaseSnapshot {
    device: u64,
    inode: u64,
    size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

fn validate_local_database(database: &Path) -> Result<(), BackendError> {
    if database.join("local/ALPM_DB_VERSION").is_file() {
        Ok(())
    } else {
        Err(BackendError(
            "本机 pacman 数据库缺少安装记录格式标识".into(),
        ))
    }
}

fn snapshot_sync(
    config: &Config,
    database: &Path,
) -> Result<Vec<Option<DatabaseSnapshot>>, BackendError> {
    let dir = database.join("sync");
    config
        .repositories
        .iter()
        .flat_map(|repo| {
            [".db", ".db.sig"]
                .into_iter()
                .map(|suffix| dir.join(format!("{}{suffix}", repo.name)))
        })
        .map(|file| match fs::metadata(&file) {
            Ok(meta) => Ok(Some(DatabaseSnapshot {
                device: meta.dev(),
                inode: meta.ino(),
                size: meta.size(),
                modified: (meta.mtime(), meta.mtime_nsec()),
                changed: (meta.ctime(), meta.ctime_nsec()),
            })),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(BackendError(format!(
                "无法核对同步数据库 {}：{error}",
                file.display()
            ))),
        })
        .collect()
}

impl Alpm {
    fn configure_execution(&self, config: &Config, cache: &Path) -> Result<(), BackendError> {
        let native = &self.native;
        let handle = self.handle.as_ptr();
        let cache = as_c(cache.as_os_str().as_bytes())?;
        // SAFETY: setters copy strings and all options belong to this handle.
        unsafe {
            self.check(
                (native.alpm_option_add_cachedir)(handle, cache.as_ptr()),
                "设置事务缓存目录",
            )?;
            self.check(
                (native.alpm_option_set_checkspace)(handle, i32::from(config.check_space)),
                "设置容量核查",
            )?;
            self.check(
                (native.alpm_option_set_usesyslog)(handle, i32::from(config.use_syslog)),
                "设置系统日志",
            )?;
            if config.parallel_downloads > 0 {
                self.check(
                    (native.alpm_option_set_parallel_downloads)(handle, config.parallel_downloads),
                    "设置并行下载",
                )?;
            }
            for dir in &config.hook_dirs {
                let dir = as_c(dir.as_os_str().as_bytes())?;
                self.check(
                    (native.alpm_option_add_hookdir)(handle, dir.as_ptr()),
                    "设置事务 hook 目录",
                )?;
            }
            for pattern in &config.no_extract {
                let pattern = as_c(pattern)?;
                self.check(
                    (native.alpm_option_add_noextract)(handle, pattern.as_ptr()),
                    "设置跳过提取路径",
                )?;
            }
            for pattern in &config.no_upgrade {
                let pattern = as_c(pattern)?;
                self.check(
                    (native.alpm_option_add_noupgrade)(handle, pattern.as_ptr()),
                    "设置保留已有文件",
                )?;
            }
        }
        Ok(())
    }
}
