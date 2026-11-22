//! Query Paru as a non-root build account without preparing or installing packages.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    process::Command,
};

use dek_core::AurBuildDependencies;
use dek_target::backend::BackendError;
use rustix::process::geteuid;

use crate::accounts::{UserAccount, user_account, user_account_by_uid};

use super::valid_package_name;

const BUILD_USER_FILE: &str = "/etc/dek/aur-build-user";
const BUILD_PATH: &str = "/usr/local/bin:/usr/bin";
const TOOLS: &[&str] = &["git", "makepkg", "make", "gcc", "fakeroot", "gpg", "vercmp"];

pub(super) struct BuildEnvironment {
    account: UserAccount,
    helper: PathBuf,
}

pub(super) struct Candidate {
    pub version: String,
    pub dependencies: AurBuildDependencies,
}

impl BuildEnvironment {
    pub fn discover() -> Result<Self, BackendError> {
        let helper =
            executable("paru").ok_or_else(|| BackendError("目标机缺少 AUR helper paru".into()))?;
        let missing: Vec<_> = TOOLS
            .iter()
            .filter(|name| executable(name).is_none())
            .copied()
            .collect();
        if !missing.is_empty() {
            return Err(BackendError(format!(
                "目标机缺少 AUR 构建工具：{}",
                missing.join(", ")
            )));
        }

        let uid = geteuid().as_raw();
        let account = if uid == 0 {
            let name = configured_build_user()?;
            user_account(&name)?
                .ok_or_else(|| BackendError(format!("目标机没有配置的 AUR 构建用户 {name:?}")))?
        } else {
            user_account_by_uid(uid)?
                .ok_or_else(|| BackendError(format!("无法定位 UID {uid} 的 AUR 构建用户")))?
        };
        if account.uid == 0 || !account.home.is_absolute() || !account.home.is_dir() {
            return Err(BackendError(format!(
                "AUR 构建需要非 root 用户及有效的主目录，当前配置为 {:?}",
                account.name
            )));
        }
        Ok(Self { account, helper })
    }

    pub fn command(&self) -> Command {
        let mut command;
        if geteuid().as_raw() == self.account.uid {
            command = Command::new(&self.helper);
        } else {
            let groups = self
                .account
                .groups
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",");
            command = Command::new("/usr/bin/setpriv");
            command.args([
                "--reuid",
                &self.account.uid.to_string(),
                "--regid",
                &self.account.gid.to_string(),
                "--groups",
                &groups,
                "--",
            ]);
            command.arg(&self.helper);
        }
        command
            .env_clear()
            .env("PATH", BUILD_PATH)
            .env("LC_ALL", "C")
            .env("HOME", &self.account.home)
            .env("USER", &self.account.name)
            .env("LOGNAME", &self.account.name)
            .current_dir(&self.account.home);
        command
    }

    pub fn candidates(
        &self,
        names: &BTreeSet<&str>,
    ) -> Result<BTreeMap<String, Candidate>, BackendError> {
        let output = self
            .command()
            .args(["-Si", "--aur", "--color", "never", "--"])
            .args(names)
            .output()
            .map_err(|err| BackendError(format!("无法查询 paru 的受管 AUR 候选：{err}")))?;
        if !output.status.success() {
            return Err(BackendError(format!(
                "paru 查询 AUR 候选失败：{}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let text = std::str::from_utf8(&output.stdout)
            .map_err(|err| BackendError(format!("paru 的 AUR 候选结果不是 UTF-8：{err}")))?;
        let candidates = parse_candidates(text)?;
        if candidates
            .keys()
            .map(String::as_str)
            .ne(names.iter().copied())
        {
            return Err(BackendError(format!(
                "paru 未准确返回受管 AUR 候选：请求 {}，返回 {}",
                names.iter().copied().collect::<Vec<_>>().join(", "),
                candidates
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        Ok(candidates)
    }
}

pub(super) fn installed_is_older(installed: &str, candidate: &str) -> Result<bool, BackendError> {
    let output = Command::new("/usr/bin/vercmp")
        .args([installed, candidate])
        .env("LC_ALL", "C")
        .output()
        .map_err(|err| BackendError(format!("无法比较 AUR 包版本：{err}")))?;
    let version_order = std::str::from_utf8(&output.stdout)
        .ok()
        .and_then(|text| text.trim().parse::<i32>().ok());
    match (output.status.success(), version_order) {
        (true, Some(order)) => Ok(order < 0),
        _ => Err(BackendError(format!(
            "无法比较 AUR 包版本 {installed:?} 和 {candidate:?}"
        ))),
    }
}

fn executable(name: &str) -> Option<PathBuf> {
    BUILD_PATH
        .split(':')
        .map(|dir| Path::new(dir).join(name))
        .find(|path| {
            fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.mode() & 0o111 != 0)
        })
}

fn configured_build_user() -> Result<String, BackendError> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(BUILD_USER_FILE)
        .map_err(|err| {
            BackendError(format!(
                "目标机缺少可用的非 root AUR 构建用户，请检查 {BUILD_USER_FILE}：{err}"
            ))
        })?;
    let metadata = file
        .metadata()
        .map_err(|err| BackendError(format!("读取 {BUILD_USER_FILE}：{err}")))?;
    if !metadata.is_file() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        return Err(BackendError(format!(
            "AUR 构建用户配置 {BUILD_USER_FILE} 需要由 root 持有且仅允许 root 写入"
        )));
    }
    let mut text = String::new();
    file.take(1025)
        .read_to_string(&mut text)
        .map_err(|err| BackendError(format!("读取 {BUILD_USER_FILE}：{err}")))?;
    let name = text.trim();
    if name.is_empty() || text.len() > 1024 || name.contains(char::is_whitespace) {
        return Err(BackendError(format!(
            "{BUILD_USER_FILE} 应包含一个非 root AUR 构建用户名"
        )));
    }
    Ok(name.into())
}

fn parse_candidates(text: &str) -> Result<BTreeMap<String, Candidate>, BackendError> {
    let mut candidates = BTreeMap::new();
    for record in text
        .split("\n\n")
        .filter(|record| !record.trim().is_empty())
    {
        let mut fields = BTreeMap::<&str, String>::new();
        let mut last = None;
        for line in record.lines() {
            if line.starts_with(char::is_whitespace) {
                if let Some(key) = last {
                    let field = fields
                        .get_mut(key)
                        .expect("last output field is registered");
                    field.push(' ');
                    field.push_str(line.trim());
                }
            } else if let Some((key, value)) = line.split_once(':') {
                let key = key.trim_end();
                if fields.insert(key, value.trim().into()).is_some() {
                    return Err(BackendError(format!("paru 返回重复字段 {key:?}")));
                }
                last = Some(key);
            }
        }
        let invalid = || {
            BackendError(format!(
                "paru 返回无效的 AUR 候选记录：{}",
                record.lines().take(3).collect::<Vec<_>>().join("；")
            ))
        };
        let Some("aur") = fields.get("Repository").map(String::as_str) else {
            return Err(invalid());
        };
        let name = fields
            .get("Name")
            .filter(|name| valid_package_name(name))
            .ok_or_else(invalid)?;
        let version = fields
            .get("Version")
            .filter(|version| !version.is_empty() && !version.contains(char::is_whitespace))
            .ok_or_else(invalid)?;
        let dependencies = |key| {
            let mut names = fields
                .get(key)
                .map(|list| {
                    list.split_ascii_whitespace()
                        .filter(|name| *name != "None")
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            names.sort_unstable();
            names.dedup();
            names
        };
        let candidate = Candidate {
            version: version.clone(),
            dependencies: AurBuildDependencies {
                runtime: dependencies("Depends On"),
                build: dependencies("Make Deps"),
                check: dependencies("Check Deps"),
            },
        };
        if candidates.insert(name.clone(), candidate).is_some() {
            return Err(BackendError(format!("paru 返回重复的 AUR 候选 {name}")));
        }
    }
    Ok(candidates)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_exact_candidates_and_wrapped_dependency_fields() {
        let text = "\
Repository      : aur
Name            : first
Version         : 2:1.2-3
Description     : A fixture
Depends On      : glibc  libfoo>=2
                  libbar
Make Deps       : gcc  make
Check Deps      : None

Repository      : aur
Name            : second
Version         : 1-1
Depends On      : None
Make Deps       : None
Check Deps      : bash
";
        let found = parse_candidates(text).unwrap();
        assert_eq!(found["first"].version, "2:1.2-3");
        assert_eq!(
            found["first"].dependencies.runtime,
            ["glibc", "libbar", "libfoo>=2"]
        );
        assert_eq!(found["first"].dependencies.build, ["gcc", "make"]);
        assert!(found["first"].dependencies.check.is_empty());
        assert_eq!(found["second"].dependencies.check, ["bash"]);
        assert!(parse_candidates(&format!("{text}\n{text}")).is_err());
        assert!(parse_candidates("Repository : core\nName : first\nVersion : 1-1").is_err());
        assert!(parse_candidates("Repository : aur\nName : first").is_err());
    }
}
