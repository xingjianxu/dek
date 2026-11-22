//! 配置仓库清单、发现及主机选择。

mod init;
mod manifest;

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use thiserror::Error;

pub use init::{EntryLanguage, initialize};
pub use manifest::{HostEntry, InputPolicy, Manifest, ModuleEntry, ProfileEntry, Target};

const MANIFEST_FILE: &str = "dek.json";

#[derive(Debug, Error)]
pub enum RepoError {
    #[error("{action} {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("配置清单 {path}：{message}")]
    InvalidManifest { path: PathBuf, message: String },
    #[error("从 {0} 向上找不到 dek.json；请指定 --repo <目录>")]
    NotFound(PathBuf),
    #[error("{message}；可选主机：{available}")]
    HostSelection { message: String, available: String },
    #[error("初始化冲突，路径已存在：{0}")]
    InitConflict(PathBuf),
    #[error("当前主机名 {0:?} 无法用作入口目录名")]
    InvalidHostname(String),
    #[error("{original}；清理本次创建的入口 {entry} 时失败：{source}")]
    InitRollback {
        entry: PathBuf,
        original: Box<RepoError>,
        #[source]
        source: io::Error,
    },
}

impl RepoError {
    fn io(action: &'static str, path: &Path, source: io::Error) -> Self {
        Self::Io {
            action,
            path: path.to_owned(),
            source,
        }
    }
}

/// 已加载的清单与其仓库根目录。清单字段中的路径相对于 [Self::root]。
#[derive(Debug)]
pub struct Repository {
    root: PathBuf,
    manifest: Manifest,
}

/// 显式名称按清单名称查找，本机自动匹配仅考虑标记为本地的条目。
#[derive(Debug, Clone, Copy)]
pub enum HostSelector<'a> {
    Named(&'a str),
    Local(&'a str),
}

impl Repository {
    /// 从指定目录加载清单。
    pub fn load(root: &Path) -> Result<Self, RepoError> {
        let root =
            fs::canonicalize(root).map_err(|err| RepoError::io("无法定位仓库目录", root, err))?;
        let path = root.join(MANIFEST_FILE);
        let contents = fs::read_to_string(&path)
            .map_err(|err| RepoError::io("无法读取配置清单", &path, err))?;
        let manifest = Manifest::parse(&path, &contents)?;
        Ok(Self { root, manifest })
    }

    /// 从起点向上发现清单，或直接读取相对于起点的显式目录。
    pub fn discover(start: &Path, explicit: Option<&Path>) -> Result<Self, RepoError> {
        if let Some(root) = explicit {
            return Self::load(&start.join(root));
        }

        let start =
            fs::canonicalize(start).map_err(|err| RepoError::io("无法定位查找目录", start, err))?;
        for root in start.ancestors() {
            let path = root.join(MANIFEST_FILE);
            match fs::symlink_metadata(&path) {
                Ok(_) => return Self::load(root),
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => return Err(RepoError::io("无法检查配置清单", &path, err)),
            }
        }
        Err(RepoError::NotFound(start))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// 返回清单中的名称与入口。本机主机名匹配忽略 ASCII 大小写。
    pub fn select_host(&self, selector: HostSelector<'_>) -> Result<(&str, &HostEntry), RepoError> {
        let message = match selector {
            HostSelector::Named(name) => {
                if let Some((name, host)) = self.manifest.hosts.get_key_value(name) {
                    return Ok((name, host));
                }
                format!("未知主机 {name:?}")
            }
            HostSelector::Local(hostname) => {
                let mut matches = self.manifest.hosts.iter().filter(|(name, host)| {
                    name.eq_ignore_ascii_case(hostname) && host.target == Target::Local
                });
                match (matches.next(), matches.next()) {
                    (Some((name, host)), None) => return Ok((name, host)),
                    (None, _) => format!("没有匹配当前主机名 {hostname:?} 的本地入口"),
                    _ => format!("当前主机名 {hostname:?} 匹配多条本地入口"),
                }
            }
        };
        let available = if self.manifest.hosts.is_empty() {
            "（无）".to_owned()
        } else {
            self.manifest
                .hosts
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        };
        Err(RepoError::HostSelection { message, available })
    }
}
