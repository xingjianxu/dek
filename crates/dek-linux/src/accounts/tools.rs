use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use dek_core::{GroupResource, UserKind, UserObservation, UserResource};
use dek_target::backend::BackendError;

const TOOL_PATHS: [&str; 5] = [
    "/usr/bin",
    "/usr/sbin",
    "/bin",
    "/sbin",
    "/run/current-system/sw/bin",
];

pub(crate) fn tool(name: &str) -> Result<PathBuf, BackendError> {
    TOOL_PATHS
        .into_iter()
        .map(|directory| Path::new(directory).join(name))
        .find(|path| path.is_file())
        .ok_or_else(|| BackendError(format!("缺少目标账户管理工具 {name}")))
}

pub(crate) fn default_home(user: &str) -> Result<PathBuf, BackendError> {
    let output = Command::new(tool("useradd")?)
        .arg("--defaults")
        .env_clear()
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .map_err(|err| BackendError(format!("读取 useradd 默认 home：{err}")))?;
    if output.status.success()
        && let Ok(settings) = std::str::from_utf8(&output.stdout)
        && let Some(home) = settings.lines().find_map(|line| line.strip_prefix("HOME="))
    {
        return Ok(Path::new(home).join(user));
    }
    Err(BackendError(
        "无法读取 useradd 默认 home；请声明账户 home".into(),
    ))
}

pub(super) fn create_group(group: &GroupResource) -> Result<(), BackendError> {
    let mut args = Vec::new();
    if let Some(gid) = group.gid {
        option(&mut args, "--gid", &gid.to_string());
    }
    run("groupadd", args, &group.name)
}

pub(super) fn create_user(
    user: &UserResource,
    primary_gid: Option<u32>,
) -> Result<(), BackendError> {
    let mut args = vec!["--no-create-home".into()];
    if user.kind == Some(UserKind::System) {
        args.push("--system".into());
    }
    if let Some(uid) = user.uid {
        option(&mut args, "--uid", &uid.to_string());
    }
    if let Some(gid) = primary_gid {
        option(&mut args, "--gid", &gid.to_string());
    } else {
        args.push("--no-user-group".into());
    }
    if let Some(groups) = user
        .extra_groups
        .as_ref()
        .filter(|groups| !groups.is_empty())
    {
        option(&mut args, "--groups", &groups.join(","));
    }
    for (flag, value) in [
        ("--home-dir", &user.home),
        ("--shell", &user.shell),
        ("--comment", &user.description),
    ] {
        if let Some(value) = value {
            option(&mut args, flag, value);
        }
    }
    run("useradd", args, &user.name)
}

pub(super) fn modify_user(
    user: &UserResource,
    actual: &UserObservation,
) -> Result<(), BackendError> {
    let mut args = Vec::new();
    for (flag, desired, current) in [
        ("--home", &user.home, &actual.home),
        ("--shell", &user.shell, &actual.shell),
        ("--comment", &user.description, &actual.description),
    ] {
        if desired != current
            && let Some(value) = desired
        {
            option(&mut args, flag, value);
        }
    }
    if let Some(groups) = &user.extra_groups {
        let missing: Vec<_> = groups
            .iter()
            .filter(|group| {
                !actual
                    .extra_groups
                    .as_ref()
                    .is_some_and(|members| members.contains(*group))
            })
            .cloned()
            .collect();
        if !missing.is_empty() {
            args.push("--append".into());
            option(&mut args, "--groups", &missing.join(","));
        }
    }
    if args.is_empty() {
        return Ok(());
    }
    run("usermod", args, &user.name)
}

fn option(args: &mut Vec<String>, flag: &str, value: &str) {
    args.push(format!("{flag}={value}"));
}

fn run(name: &str, args: Vec<String>, account: &str) -> Result<(), BackendError> {
    let output = Command::new(tool(name)?)
        .args(args)
        .arg("--")
        .arg(account)
        .env_clear()
        .env("PATH", TOOL_PATHS.join(":"))
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .map_err(|err| BackendError(format!("启动 {name} 管理 {account:?}：{err}")))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(BackendError(format!(
            "{name} 管理 {account:?} 失败（{}）：{}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}
