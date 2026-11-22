use std::{
    ffi::OsString,
    fs,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::{Component, Path, PathBuf},
    process::{Command, Output},
};

use dek_target::backend::BackendError;
use rustix::{
    process::{Gid, Uid, getegid, geteuid},
    thread::{set_thread_gid, set_thread_groups, set_thread_uid},
};

use crate::accounts::{UserAccount, user_account};

pub(super) fn resolve(name: &str) -> Result<Option<UserAccount>, BackendError> {
    let account = user_account(name)?;
    if let Some(account) = &account
        && (account.home.file_name().is_none()
            || !account.home.is_absolute()
            || account
                .home
                .components()
                .any(|part| part == Component::ParentDir))
    {
        return Err(BackendError(format!("用户 {name:?} 的 home 路径无效")));
    }
    Ok(account)
}

pub(super) fn config_dir(account: &UserAccount) -> PathBuf {
    account.home.join(".config/systemd/user")
}

pub(super) fn with_permissions<T: Send>(
    account: &UserAccount,
    operation: impl FnOnce() -> Result<T, BackendError> + Send,
) -> Result<T, BackendError> {
    let uid = Uid::from_raw(account.uid);
    let gid = Gid::from_raw(account.gid);
    if geteuid() == uid && getegid() == gid {
        return operation();
    }
    // Use Linux per-thread credential calls in a thread that exits after the operation.
    std::thread::scope(|scope| {
        scope
            .spawn(move || {
                set_thread_groups(&[])
                    .and_then(|()| set_thread_gid(gid))
                    .and_then(|()| set_thread_uid(uid))
                    .map_err(|err| {
                        BackendError(format!(
                            "无法以用户 {:?} 的权限访问单元文件：{err}",
                            account.name
                        ))
                    })?;
                operation()
            })
            .join()
            .map_err(|_| BackendError("用户单元文件作业异常退出".into()))?
    })
}

pub(super) fn online(account: &UserAccount) -> Result<bool, BackendError> {
    let socket = runtime_dir(account).join("systemd/private");
    match fs::symlink_metadata(&socket) {
        Ok(stat) if stat.file_type().is_socket() && stat.uid() == account.uid => Ok(true),
        Ok(_) => Err(BackendError(format!("用户 manager 套接字无效 {socket:?}"))),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(BackendError(format!(
            "无法访问用户 manager 套接字 {socket:?}：{err}"
        ))),
    }
}

pub(super) fn command(
    account: &UserAccount,
    disk_only: bool,
    args: &[&str],
) -> Result<Output, BackendError> {
    let mut command;
    let env = environment(account);
    if geteuid().as_raw() == account.uid {
        command = Command::new("/usr/bin/env");
        command.arg("-i");
    } else {
        command = Command::new("/usr/bin/runuser");
        command.args(["-u", &account.name, "--", "/usr/bin/env", "-i"]);
    }
    command
        .env_clear()
        .args(env)
        .args(["/usr/bin/systemctl", "--user", "--no-pager"]);
    if disk_only {
        command.arg("--root=/");
    }
    command.args(args).output().map_err(|err| {
        BackendError(format!(
            "无法为用户 {:?} 启动 systemctl：{err}",
            account.name
        ))
    })
}

fn runtime_dir(account: &UserAccount) -> PathBuf {
    Path::new("/run/user").join(account.uid.to_string())
}

fn environment(account: &UserAccount) -> [OsString; 5] {
    let mut home = OsString::from("HOME=");
    home.push(&account.home);
    let mut runtime = OsString::from("XDG_RUNTIME_DIR=");
    runtime.push(runtime_dir(account));
    let mut bus = OsString::from("DBUS_SESSION_BUS_ADDRESS=unix:path=");
    bus.push(runtime_dir(account).join("bus"));
    [
        home,
        runtime,
        bus,
        "PATH=/usr/bin".into(),
        "LC_ALL=C".into(),
    ]
}
