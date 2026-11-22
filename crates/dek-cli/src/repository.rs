use std::{env, error::Error, path::Path};

use dek_controller::repo::{HostSelector, Repository};

pub(crate) fn discover_repo(explicit: Option<&Path>) -> Result<Repository, Box<dyn Error>> {
    Ok(Repository::discover(&env::current_dir()?, explicit)?)
}

pub(crate) fn local_hostname() -> Result<String, Box<dyn Error>> {
    hostname::get()
        .map_err(|err| format!("无法读取当前主机名：{err}"))?
        .into_string()
        .map_err(|name| format!("当前主机名不是有效的 UTF-8：{name:?}").into())
}

pub(crate) enum HostSelection {
    Named(String),
    Local(String),
}

impl HostSelection {
    pub(crate) fn from_arg(explicit: Option<String>) -> Result<Self, Box<dyn Error>> {
        match explicit {
            Some(name) => Ok(Self::Named(name)),
            None => Ok(Self::Local(local_hostname()?)),
        }
    }

    pub(crate) fn selector(&self) -> HostSelector<'_> {
        match self {
            Self::Named(name) => HostSelector::Named(name),
            Self::Local(name) => HostSelector::Local(name),
        }
    }
}
