//! Refresh and validate repository metadata while holding the live pacman lock.

use std::{
    fs::{self, File},
    io,
    os::unix::fs::PermissionsExt,
    path::Path,
};

use dek_target::backend::BackendError;
use rustix::fs::{CWD, RenameFlags, renameat_with};
use tempfile::TempDir;

use super::{Alpm, Config, copy_local, db_error, snapshot_public_keyring};

pub(super) struct UpgradeDatabase {
    stage: TempDir,
}

impl UpgradeDatabase {
    pub(super) fn refresh(config: &Config) -> Result<Self, BackendError> {
        let stage = tempfile::Builder::new()
            .prefix(".dek-upgrade-")
            .tempdir_in(&config.db)
            .map_err(db_error)?;
        let root = stage.path();
        // ALPM's download user needs to traverse the staging directory. The
        // root account retains exclusive write access to the database paths.
        fs::set_permissions(root, fs::Permissions::from_mode(0o711)).map_err(db_error)?;
        copy_local(&config.db.join("local"), &root.join("local")).map_err(db_error)?;
        let sync = root.join("sync");
        let live_sync = config.db.join("sync");
        match fs::symlink_metadata(&live_sync) {
            Ok(metadata) if metadata.is_dir() => {
                // Keep unrelated metadata (including .files) and repos with
                // Sync disabled while refreshing the configured Sync repos.
                copy_local(&live_sync, &sync).map_err(db_error)?;
                fs::set_permissions(&sync, metadata.permissions()).map_err(db_error)?;
            }
            Ok(_) => return Err(BackendError("pacman 同步数据库路径需要真实目录".into())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&sync).map_err(db_error)?;
            }
            Err(error) => return Err(db_error(error)),
        }
        let gpg = root.join("gnupg");
        snapshot_public_keyring(&config.gpg, &gpg).map_err(db_error)?;
        let alpm = Alpm::open(config, root, &gpg, &root.join("refresh.log"))?;
        alpm.refresh(config, &sync)?;
        // Preserve the exact database the native prepared handle is using.
        // Publish an equivalent copy under the live lock before commit.
        let publication = root.join("publish");
        copy_local(&sync, &publication).map_err(db_error)?;
        fs::set_permissions(
            &publication,
            fs::metadata(&sync).map_err(db_error)?.permissions(),
        )
        .map_err(db_error)?;
        for repo in &config.repositories {
            for suffix in [".db", ".db.sig"] {
                match File::open(publication.join(format!("{}{suffix}", repo.name))) {
                    Ok(file) => file.sync_all().map_err(db_error)?,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(db_error(error)),
                }
            }
        }
        File::open(&publication)
            .and_then(|dir| dir.sync_all())
            .map_err(db_error)?;
        Ok(Self { stage })
    }

    pub(super) fn path(&self) -> &Path {
        self.stage.path()
    }

    /// Publish the full verified directory in one operation on the same volume.
    pub(super) fn publish(&self, db: &Path) -> Result<(), BackendError> {
        let staged = self.path().join("publish");
        let live = db.join("sync");
        let flags = if live.try_exists().map_err(db_error)? {
            RenameFlags::EXCHANGE
        } else {
            RenameFlags::NOREPLACE
        };
        renameat_with(CWD, &staged, CWD, &live, flags).map_err(|error| {
            BackendError(format!("无法发布已验证的 pacman 同步数据库：{error}"))
        })?;
        File::open(db)
            .and_then(|dir| dir.sync_all())
            .map_err(db_error)
    }
}
