use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use dek_target::backend::BackendError;

use super::nss::{Identity, Key, NameService, SystemNss};

pub(super) struct Database {
    pub(super) etc: PathBuf,
    pub(super) nss: Box<dyn NameService>,
}

impl Default for Database {
    fn default() -> Self {
        Self {
            etc: "/etc".into(),
            nss: Box::new(SystemNss),
        }
    }
}

#[derive(Clone)]
pub(crate) struct LocalUser {
    pub(crate) uid: u32,
    pub(crate) gid: u32,
    pub(super) description: String,
    pub(crate) home: String,
    pub(super) shell: String,
}

pub(super) struct LocalGroup {
    pub(super) gid: u32,
    pub(super) members: BTreeSet<String>,
}

pub(super) struct Snapshot<'a> {
    nss: &'a dyn NameService,
    users: BTreeMap<String, LocalUser>,
    groups: BTreeMap<String, LocalGroup>,
}

impl Database {
    pub(super) fn read(&self) -> Result<Snapshot<'_>, BackendError> {
        let users = read_users(&self.etc.join("passwd"))?;
        let groups = read_groups(&self.etc.join("group"))?;
        Ok(Snapshot {
            nss: &*self.nss,
            users,
            groups,
        })
    }
}

impl Snapshot<'_> {
    pub(super) fn ensure_unique_ssh_home(
        &self,
        name: &str,
        home: &Path,
    ) -> Result<(), BackendError> {
        if let Some((other, _)) = self
            .users
            .iter()
            .find(|(other, account)| *other != name && Path::new(&account.home) == home)
        {
            return Err(BackendError(format!(
                "用户 {name:?} 与 {other:?} 共用授权目录"
            )));
        }
        Ok(())
    }

    pub(super) fn user(
        &self,
        name: &str,
        desired_uid: Option<u32>,
    ) -> Result<Option<&LocalUser>, BackendError> {
        let local = self.users.get(name);
        validate_identity(
            "用户",
            name,
            local.map(|user| user.uid),
            desired_uid,
            |key| self.nss.user(key),
            |id| {
                self.users
                    .iter()
                    .find(|(other, user)| other.as_str() != name && user.uid == id)
                    .map(|(other, _)| other.as_str())
            },
        )?;
        Ok(local)
    }

    pub(super) fn group(
        &self,
        name: &str,
        desired_gid: Option<u32>,
    ) -> Result<Option<&LocalGroup>, BackendError> {
        let local = self.groups.get(name);
        validate_identity(
            "组",
            name,
            local.map(|group| group.gid),
            desired_gid,
            |key| self.nss.group(key),
            |id| {
                self.groups
                    .iter()
                    .find(|(other, group)| other.as_str() != name && group.gid == id)
                    .map(|(other, _)| other.as_str())
            },
        )?;
        Ok(local)
    }
}

fn validate_identity<'a>(
    kind: &str,
    name: &str,
    local: Option<u32>,
    desired: Option<u32>,
    lookup: impl Fn(Key<'_>) -> Result<Option<Identity>, BackendError>,
    other_local: impl Fn(u32) -> Option<&'a str>,
) -> Result<(), BackendError> {
    let visible = lookup(Key::Name(name))?;
    match (local, &visible) {
        (None, Some(_)) => {
            return Err(BackendError(format!(
                "{kind} {name:?} 已由外部 NSS 名称服务管理"
            )));
        }
        (Some(id), record)
            if !record
                .as_ref()
                .is_some_and(|entry| entry.name == name && entry.id == id) =>
        {
            return Err(BackendError(format!(
                "{kind} {name:?} 的本地数据库与 NSS 名称解析不一致"
            )));
        }
        _ => {}
    }
    if let (Some(actual), Some(expected)) = (local, desired)
        && actual != expected
    {
        return Err(BackendError(format!(
            "{kind} {name:?} 的数字 ID 为 {actual}，声明为 {expected}，需要单独迁移"
        )));
    }
    if let Some(id) = local.or(desired) {
        if let Some(other) = other_local(id) {
            return Err(BackendError(format!(
                "{kind} {name:?} 的数字 ID {id} 已被 {other:?} 占用"
            )));
        }
        let by_id = lookup(Key::Id(id))?;
        if let Some(other) = by_id
            .as_ref()
            .filter(|entry| entry.name != name || entry.id != id)
        {
            return Err(BackendError(format!(
                "{kind} {name:?} 的数字 ID {id} 已由 NSS 条目 {:?} 占用",
                other.name
            )));
        }
        if local.is_some() && by_id.is_none() {
            return Err(BackendError(format!(
                "{kind} {name:?} 的数字 ID {id} 无法经 NSS 解析"
            )));
        }
    }
    Ok(())
}

fn read_users(path: &Path) -> Result<BTreeMap<String, LocalUser>, BackendError> {
    let mut users = BTreeMap::new();
    for (number, line) in read(path)?
        .lines()
        .enumerate()
        .filter(|(_, line)| actual_entry(line))
    {
        let [name, _, uid, gid, description, home, shell] = fields(line, path, number)?;
        let user = LocalUser {
            uid: numeric(uid, path, number)?,
            gid: numeric(gid, path, number)?,
            description: description.into(),
            home: home.into(),
            shell: shell.into(),
        };
        if users.insert(name.into(), user).is_some() {
            return Err(invalid_row(path, number, "用户名称重复"));
        }
    }
    Ok(users)
}

fn read_groups(path: &Path) -> Result<BTreeMap<String, LocalGroup>, BackendError> {
    let mut groups: BTreeMap<String, LocalGroup> = BTreeMap::new();
    for (number, line) in read(path)?
        .lines()
        .enumerate()
        .filter(|(_, line)| actual_entry(line))
    {
        let [name, _, gid, members] = fields(line, path, number)?;
        let gid = numeric(gid, path, number)?;
        let group = groups.entry(name.into()).or_insert_with(|| LocalGroup {
            gid,
            members: BTreeSet::new(),
        });
        if group.gid != gid {
            return Err(invalid_row(path, number, "同名组具有不同的 GID"));
        }
        group.members.extend(
            members
                .split(',')
                .filter(|member| !member.is_empty())
                .map(str::to_owned),
        );
    }
    Ok(groups)
}

fn read(path: &Path) -> Result<String, BackendError> {
    fs::read_to_string(path)
        .map_err(|err| BackendError(format!("读取本地账户数据库 {}：{err}", path.display())))
}

fn actual_entry(line: &str) -> bool {
    !line.is_empty() && !line.starts_with(['#', '+', '-'])
}

fn fields<'a, const N: usize>(
    line: &'a str,
    path: &Path,
    number: usize,
) -> Result<[&'a str; N], BackendError> {
    let mut parts = line.split(':');
    let values = std::array::from_fn(|_| parts.next());
    if values.iter().any(Option::is_none) || parts.next().is_some() || values[0] == Some("") {
        return Err(invalid_row(path, number, "无效账户记录"));
    }
    Ok(values.map(Option::unwrap))
}

fn numeric(value: &str, path: &Path, number: usize) -> Result<u32, BackendError> {
    value
        .parse()
        .map_err(|_| invalid_row(path, number, "无效数字 ID"))
}

fn invalid_row(path: &Path, number: usize, reason: &str) -> BackendError {
    BackendError(format!("{} 第 {} 行：{reason}", path.display(), number + 1))
}
