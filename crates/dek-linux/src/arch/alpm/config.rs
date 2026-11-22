use std::{path::PathBuf, process::Command};

use dek_target::backend::BackendError;

pub(super) const SYNC: i32 = 1;
pub(super) const INSTALL: i32 = 1 << 2;
pub(super) const UPGRADE: i32 = 1 << 3;
const ALL: i32 = 15;
const INHERIT_SIGNATURES: i32 = 1 << 30;

#[derive(Default)]
pub(super) struct Config {
    pub root: PathBuf,
    pub db: PathBuf,
    pub gpg: PathBuf,
    pub signatures: i32,
    pub architectures: Vec<String>,
    pub ignored_packages: Vec<String>,
    pub ignored_groups: Vec<String>,
    pub download_user: Option<String>,
    pub disable_timeout: bool,
    pub disable_sandbox_filesystem: bool,
    pub disable_sandbox_syscalls: bool,
    pub external_download: bool,
    pub repositories: Vec<Repository>,
}

pub(super) struct Repository {
    pub name: String,
    pub usage: i32,
    pub signatures: i32,
    pub servers: Vec<String>,
}

impl Config {
    pub fn read() -> Result<Self, BackendError> {
        let output = Command::new("/usr/bin/pacman-conf")
            .env("LC_ALL", "C")
            .output()
            .map_err(|error| BackendError(format!("无法读取目标 pacman 配置：{error}")))?;
        if !output.status.success() || !output.stderr.is_empty() {
            return Err(BackendError(format!(
                "无法完整读取目标 pacman 配置：{}",
                String::from_utf8_lossy(&output.stderr).trim(),
            )));
        }
        let text = std::str::from_utf8(&output.stdout)
            .map_err(|error| BackendError(format!("pacman 配置输出不是 UTF-8：{error}")))?;
        Self::parse(text)
    }

    fn parse(text: &str) -> Result<Self, BackendError> {
        let mut config = Self::default();
        let mut repos = false;
        for line in text.lines() {
            if let Some(name) = line
                .strip_prefix('[')
                .and_then(|line| line.strip_suffix(']'))
            {
                if name == "options" && !repos {
                    continue;
                }
                if !super::super::valid_package_name(name)
                    || config.repositories.iter().any(|repo| repo.name == name)
                {
                    return Err(BackendError(format!("无效或重复的 pacman 仓库名 {name:?}")));
                }
                repos = true;
                config.repositories.push(Repository {
                    name: name.into(),
                    usage: 0,
                    signatures: INHERIT_SIGNATURES,
                    servers: Vec::new(),
                });
                continue;
            }
            let (key, value) = line.split_once(" = ").unwrap_or((line, ""));
            if let Some(repo) = config.repositories.last_mut() {
                match key {
                    "Usage" => repo.usage |= usage(value)?,
                    "SigLevel" => signature_flag(&mut repo.signatures, value)?,
                    "Server" => repo.servers.push(value.into()),
                    _ => {}
                }
            } else {
                match key {
                    "RootDir" => config.root = value.into(),
                    "DBPath" => config.db = value.into(),
                    "GPGDir" => config.gpg = value.into(),
                    "Architecture" => config.architectures.push(value.into()),
                    "IgnorePkg" => config.ignored_packages.push(value.into()),
                    "IgnoreGroup" => config.ignored_groups.push(value.into()),
                    "SigLevel" => signature_flag(&mut config.signatures, value)?,
                    "DownloadUser" => config.download_user = Some(value.into()),
                    "DisableDownloadTimeout" => config.disable_timeout = true,
                    "DisableSandbox" => {
                        config.disable_sandbox_filesystem = true;
                        config.disable_sandbox_syscalls = true;
                    }
                    "DisableSandboxFilesystem" => config.disable_sandbox_filesystem = true,
                    "DisableSandboxSyscalls" => config.disable_sandbox_syscalls = true,
                    "XferCommand" => config.external_download = true,
                    _ => {}
                }
            }
        }
        if [&config.root, &config.db, &config.gpg]
            .iter()
            .any(|path| !path.is_absolute())
        {
            return Err(BackendError(
                "pacman 配置缺少绝对 RootDir、DBPath 或 GPGDir".into(),
            ));
        }
        if config.repositories.is_empty() {
            return Err(BackendError("pacman 配置中没有软件仓库".into()));
        }
        for repo in &mut config.repositories {
            if repo.usage == 0 {
                repo.usage = ALL;
            }
        }
        Ok(config)
    }
}

fn usage(value: &str) -> Result<i32, BackendError> {
    match value {
        "All" => Ok(ALL),
        "Sync" => Ok(SYNC),
        "Search" => Ok(1 << 1),
        "Install" => Ok(INSTALL),
        "Upgrade" => Ok(UPGRADE),
        _ => Err(BackendError(format!("未知的 pacman 仓库 Usage {value:?}"))),
    }
}

fn signature_flag(flags: &mut i32, value: &str) -> Result<(), BackendError> {
    // pacman-conf prints separately normalized package and database policies.
    let (kind, level) = value
        .strip_prefix("Package")
        .map(|value| (0, value))
        .or_else(|| value.strip_prefix("Database").map(|value| (10, value)))
        .ok_or_else(|| BackendError(format!("未知的 pacman 仓库 SigLevel {value:?}")))?;
    *flags &= !INHERIT_SIGNATURES;
    let bits = match level {
        "Never" | "TrustedOnly" => 0,
        "Required" => 1,
        "Optional" => 0b11,
        "TrustAll" => 0b1100,
        _ => {
            return Err(BackendError(format!(
                "未知的 pacman 仓库 SigLevel {value:?}"
            )));
        }
    };
    *flags |= bits << kind;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_pacman_conf_repository_order_and_effective_settings() {
        let config = Config::parse("[options]\nRootDir = /\nDBPath = /var/lib/pacman\nGPGDir = /etc/pacman.d/gnupg\nArchitecture = x86_64\nIgnorePkg = pinned\nSigLevel = PackageRequired\nSigLevel = DatabaseOptional\n[custom]\nUsage = Install\nServer = file:///tmp/custom\n[core]\nUsage = All\nSigLevel = PackageNever\nSigLevel = DatabaseRequired\nServer = https://mirror.invalid/core\n").unwrap();
        let [custom, core] = config.repositories.as_slice() else {
            panic!("expected both repositories");
        };
        assert_eq!(config.signatures, 1 | (3 << 10));
        assert_eq!(config.ignored_packages, ["pinned"]);
        assert_eq!(custom.name, "custom");
        assert_eq!(custom.usage, INSTALL);
        assert_eq!(custom.signatures, INHERIT_SIGNATURES);
        assert_eq!(core.signatures, 1 << 10);
        assert_eq!(core.servers, ["https://mirror.invalid/core"]);
    }
}
