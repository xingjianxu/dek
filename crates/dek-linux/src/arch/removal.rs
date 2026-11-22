//! A private ALPM pre-transaction hook closes the check/remove race.

use std::{
    fs,
    io::{self, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
};

use dek_core::PackageId;
use dek_target::backend::BackendError;
use rustix::process::geteuid;
use tempfile::{Builder, TempDir};

const RUNTIME: &str = "/run";

pub(super) fn prepare(package: &PackageId, version: &str) -> Result<TempDir, BackendError> {
    if !geteuid().is_root() {
        return Err(BackendError(format!(
            "移除软件包 {} 需要目标端 root 权限",
            package.name
        )));
    }
    let metadata = fs::symlink_metadata(RUNTIME).map_err(guard_error)?;
    if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        return Err(BackendError(format!(
            "pacman 事务守卫需要可信的 {RUNTIME} 目录"
        )));
    }
    let mode = fs::Permissions::from_mode(0o700);
    let directory = Builder::new()
        .prefix("dek-pacman-")
        .permissions(mode.clone())
        .tempdir_in(RUNTIME)
        .map_err(guard_error)?;
    fs::set_permissions(directory.path(), mode).map_err(guard_error)?;
    let helper = directory.path().join("verify-removal");
    let expected = directory.path().join("reviewed-package");
    write_private(&helper, include_bytes!("verify-removal.sh")).map_err(guard_error)?;
    write_private(
        &expected,
        format!("{}\n{version}\n", package.name).as_bytes(),
    )
    .map_err(guard_error)?;
    let hook = format!(
        "[Trigger]\nOperation = Remove\nType = Package\nTarget = *\n\n[Action]\nDescription = Checking reviewed dek removal\nWhen = PreTransaction\nAbortOnFail\nNeedsTargets\nExec = /usr/bin/sh {} {}\n",
        helper.display(),
        expected.display()
    );
    write_private(
        &directory.path().join("00-dek-check-removal.hook"),
        hook.as_bytes(),
    )
    .map_err(guard_error)?;
    Ok(directory)
}

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(bytes)
}

fn guard_error(error: io::Error) -> BackendError {
    BackendError(format!("无法准备 pacman 事务守卫：{error}"))
}
