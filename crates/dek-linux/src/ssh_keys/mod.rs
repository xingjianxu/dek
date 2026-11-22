//! Reconcile the managed portion of a local account's OpenSSH authorization file.

mod entries;

use std::{
    io::{self, Read},
    path::{Component, Path, PathBuf},
};

use dek_core::{
    Action, ActionKind, AuthorizedKeysResource, ContentDigest, DesiredResource, FileAttributes,
    FileContent, FileObservation, Precondition, ResourceCapability, ResourceId, ResourceKind,
    ResourceSet, UserObservation, UserResource,
};
use dek_target::backend::{
    ActionContext, ActionReadiness, Backend, BackendError, PlannedAction, PlanningContext,
};
use rustix::{
    fs::{Mode, Stat, fstat},
    process::{Gid, Uid},
};

use crate::{
    accounts::{SshUser, ensure_unique_ssh_home, ssh_user, tools::default_home},
    filesystem::{
        atomic_file::{StagedFile, TargetPath, same_revision},
        create_owned_directory, digest,
    },
};
use entries::{Document, ManagedKeys};

pub struct AuthorizedKeysBackend;

impl Backend for AuthorizedKeysBackend {
    fn capabilities(&self) -> Vec<ResourceCapability> {
        vec![ResourceCapability {
            kind: ResourceKind::AuthorizedKeys,
            version: 1,
            attributes: ["keyFiles".into()].into(),
        }]
    }

    fn validate(&self, resource: &DesiredResource) -> Result<(), BackendError> {
        declaration(resource)?;
        Ok(())
    }

    fn resource_dependencies(
        &self,
        resource: &DesiredResource,
        managed: &ResourceSet,
    ) -> Result<Vec<ResourceId>, BackendError> {
        let declaration = declaration(resource)?;
        let location = resolve(declaration, managed)?;
        let home = Path::new(&location.home);
        let path = authorized_keys(home);
        let mut dependencies = Vec::new();
        for resource in &managed.resources {
            match &resource.desired {
                DesiredResource::User(user) if user.name == declaration.user => {
                    dependencies.push(resource.id());
                }
                DesiredResource::User(user) => {
                    let other = match &user.home {
                        Some(home) => PathBuf::from(home),
                        None => match ssh_user(&user.name)? {
                            Some(account) => PathBuf::from(account.home),
                            None => default_home(&user.name)?,
                        },
                    };
                    if other == home {
                        return Err(BackendError(format!(
                            "账户 {:?} 与 {:?} 的授权目录相同",
                            declaration.user, user.name
                        )));
                    }
                }
                DesiredResource::Directory(directory) if path.starts_with(&directory.path) => {
                    if Path::new(&directory.path) == path {
                        return Err(overlapping_path(&path));
                    }
                    dependencies.push(resource.id());
                }
                DesiredResource::File(file) if path.starts_with(&file.path) => {
                    return Err(overlapping_path(&path));
                }
                DesiredResource::AgeSecret(secret) if path.starts_with(&secret.path) => {
                    return Err(overlapping_path(&path));
                }
                _ => {}
            }
        }
        Ok(dependencies)
    }

    fn observe(&self, resource: &DesiredResource) -> Result<Vec<Precondition>, BackendError> {
        let declaration = declaration(resource)?;
        let location = resolve(declaration, &ResourceSet::default())?;
        let current = Snapshot::open(&location.home, location.uid)?;
        Ok(location.preconditions(&declaration.user, &current))
    }

    fn plan(
        &self,
        resource: &DesiredResource,
        observed: &[Precondition],
    ) -> Result<Vec<PlannedAction>, BackendError> {
        let declaration = declaration(resource)?;
        let location = resolve(declaration, &ResourceSet::default())?;
        let current = Snapshot::open(&location.home, location.uid)?;
        if location.preconditions(&declaration.user, &current) != observed {
            return Err(BackendError("SSH 授权在规划期间改变".into()));
        }
        plan(
            declaration,
            &location,
            &current,
            ManagedKeys::read(declaration, |digest| {
                Err(BackendError(format!("缺少已审阅内容 {}", digest.0)))
            })?,
        )
    }

    fn plan_with_contents(
        &self,
        resource: &DesiredResource,
        context: &PlanningContext<'_>,
    ) -> Result<Vec<PlannedAction>, BackendError> {
        let declaration = declaration(resource)?;
        let location = resolve(declaration, context.resources)?;
        let current = Snapshot::open(&location.home, location.uid)?;
        let keys = ManagedKeys::read(declaration, |digest| context.content(digest))?;
        plan(declaration, &location, &current, keys)
    }

    fn validate_action(&self, action: &Action) -> Result<(), BackendError> {
        Review::new(action)?;
        Ok(())
    }

    fn inspect_action(&self, action: &Action) -> Result<ActionReadiness, BackendError> {
        let review = Review::new(action)?;
        let account = review.goal.account()?;
        let current = Snapshot::open(review.goal.home, Some(account.uid))?;
        review.readiness(&current, &account)
    }

    fn execute(&self, context: &ActionContext<'_>) -> Result<(), BackendError> {
        let review = Review::new(context.action)?;
        let account = review.goal.account()?;
        let keys = ManagedKeys::read(review.goal.declaration, |digest| context.content(digest))?;
        let body = keys.body();
        if digest(&body) != *review.goal.managed_digest {
            return Err(BackendError("SSH 公钥的规范化内容与已审阅摘要不符".into()));
        }
        let mut current = Snapshot::open(review.goal.home, Some(account.uid))?;
        match review.readiness(&current, &account)? {
            ActionReadiness::Ready => {}
            ActionReadiness::Satisfied => return Ok(()),
            ActionReadiness::Conflict(reason) => return Err(BackendError(reason)),
        }
        if current.target.is_none() {
            let ssh = current.ssh.as_ref().ok_or_else(|| {
                BackendError(format!("SSH 授权需要已存在的 home {:?}", review.goal.home))
            })?;
            let ssh_name = Path::new(review.goal.home).join(".ssh");
            ssh.require_current_parent(&ssh_name)
                .map_err(|error| path_error(&ssh_name, error))?;
            create_owned_directory(ssh, account.uid, account.gid, 0o700)
                .map_err(|error| path_error(&ssh_name, error))?;
            current = Snapshot::open(review.goal.home, Some(account.uid))?;
            if review.readiness(&current, &account)? != ActionReadiness::Ready {
                return Err(BackendError("SSH 授权目录在创建后已经改变".into()));
            }
        }
        let document = Document::read(&current.bytes)?;
        document.check_manual(&keys)?;
        let bytes = document.render(&body);
        let path = authorized_keys(Path::new(review.goal.home));
        let target = current
            .target
            .as_ref()
            .ok_or_else(|| BackendError("SSH 授权目录在执行期间改变".into()))?;
        let stage = StagedFile::new(
            target,
            &bytes,
            Some(Uid::from_raw(account.uid)),
            Some(Gid::from_raw(account.gid)),
            Mode::from_bits_truncate(0o600),
        )
        .map_err(|error| path_error(&path, error))?;
        let updated = review.goal.account()?;
        if (updated.uid, updated.gid) != (account.uid, account.gid) {
            return Err(BackendError("SSH 授权账户在执行期间改变".into()));
        }
        target
            .require_current_parent(&path)
            .map_err(|error| path_error(&path, error))?;
        stage
            .publish(current.stat.as_ref())
            .map_err(|error| path_error(&path, error))
    }

    fn verify(&self, action: &ActionKind) -> Result<bool, BackendError> {
        let goal = Goal::new(action)?;
        let account = goal.account()?;
        Snapshot::open(goal.home, Some(account.uid))?
            .satisfied(goal.managed_digest, Some(account.uid))
    }
}

fn declaration(resource: &DesiredResource) -> Result<&AuthorizedKeysResource, BackendError> {
    if let DesiredResource::AuthorizedKeys(keys) = resource {
        Ok(keys)
    } else {
        Err(BackendError("SSH 公钥后端收到其他资源类型".into()))
    }
}

fn plan(
    declaration: &AuthorizedKeysResource,
    location: &Location,
    current: &Snapshot,
    keys: ManagedKeys,
) -> Result<Vec<PlannedAction>, BackendError> {
    let document = Document::read(&current.bytes)?;
    document.check_manual(&keys)?;
    let body = keys.body();
    let managed_digest = digest(&body);
    if current.satisfied(&managed_digest, location.uid)? {
        return Ok(Vec::new());
    }
    Ok(vec![PlannedAction {
        kind: ActionKind::UpdateAuthorizedKeys {
            keys: declaration.clone(),
            home: location.home.clone(),
            managed_digest,
        },
        reason: "SSH 受管授权条目或文件属性需要更新".into(),
        preconditions: location.preconditions(&declaration.user, current),
        content_changed: current.bytes != document.render(&body),
    }])
}

struct Location {
    home: String,
    uid: Option<u32>,
    existing_account: Option<UserObservation>,
}

impl Location {
    fn preconditions(&self, user: &str, current: &Snapshot) -> Vec<Precondition> {
        let mut conditions = vec![Precondition::File {
            path: authorized_keys(Path::new(&self.home))
                .to_string_lossy()
                .into_owned(),
            expected: current.observation(),
        }];
        if let Some(account) = &self.existing_account {
            conditions.push(Precondition::UserAccount {
                user: user.into(),
                expected: Some(account.clone()),
            });
        }
        conditions
    }
}

fn resolve(
    declaration: &AuthorizedKeysResource,
    managed: &ResourceSet,
) -> Result<Location, BackendError> {
    let actual = ssh_user(&declaration.user)?;
    let declared = managed
        .resources
        .iter()
        .find_map(|resource| match &resource.desired {
            DesiredResource::User(user) if user.name == declaration.user => Some(user),
            _ => None,
        });
    let home = intended_home(declared, actual.as_ref(), &declaration.user)?;
    ensure_unique_ssh_home(&declaration.user, Path::new(&home))?;
    let existing_account = match (declared, actual.as_ref()) {
        (None, Some(account)) => Some(user_observation(account, home.clone())),
        _ => None,
    };
    Ok(Location {
        home,
        uid: actual
            .map(|account| account.uid)
            .or_else(|| declared.and_then(|user| user.uid)),
        existing_account,
    })
}

fn intended_home(
    declared: Option<&UserResource>,
    actual: Option<&SshUser>,
    user: &str,
) -> Result<String, BackendError> {
    if let Some(home) = declared
        .and_then(|user| user.home.as_deref())
        .or_else(|| actual.map(|account| account.home.as_str()))
    {
        return normalize_home(Path::new(home));
    }
    if declared.is_some() {
        return normalize_home(&default_home(user)?);
    }
    Err(BackendError(format!(
        "SSH 授权需要已存在或同计划声明的本地账户 {user:?}"
    )))
}

fn normalize_home(path: &Path) -> Result<String, BackendError> {
    if !path.is_absolute()
        || path == Path::new("/")
        || path.as_os_str().as_encoded_bytes().contains(&0)
        || path
            .components()
            .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
    {
        return Err(BackendError(format!(
            "SSH 授权需要有效的绝对 home 路径 {path:?}"
        )));
    }
    path.components()
        .collect::<PathBuf>()
        .into_os_string()
        .into_string()
        .map_err(|_| BackendError("SSH 授权的 home 需要 UTF-8 路径".into()))
}

fn authorized_keys(home: &Path) -> PathBuf {
    home.join(".ssh/authorized_keys")
}

fn user_observation(account: &SshUser, home: String) -> UserObservation {
    UserObservation {
        uid: account.uid,
        primary_gid: Some(account.gid),
        home: Some(home),
        extra_groups: None,
        shell: None,
        description: None,
    }
}

struct Goal<'a> {
    declaration: &'a AuthorizedKeysResource,
    home: &'a str,
    managed_digest: &'a ContentDigest,
}

impl<'a> Goal<'a> {
    fn new(kind: &'a ActionKind) -> Result<Self, BackendError> {
        let ActionKind::UpdateAuthorizedKeys {
            keys,
            home,
            managed_digest,
        } = kind
        else {
            return Err(BackendError("SSH 公钥后端收到其他动作类型".into()));
        };
        if normalize_home(Path::new(home))? != *home {
            return Err(BackendError("SSH 授权动作的 home 路径未规范化".into()));
        }
        Ok(Self {
            declaration: keys,
            home,
            managed_digest,
        })
    }

    fn account(&self) -> Result<SshUser, BackendError> {
        let user = &self.declaration.user;
        let account = ssh_user(user)?
            .ok_or_else(|| BackendError(format!("SSH 授权的本地账户 {user:?} 尚未就绪")))?;
        if normalize_home(Path::new(&account.home))? != self.home {
            return Err(BackendError(format!(
                "SSH 授权的账户 {user:?} home 与审阅路径不符"
            )));
        }
        ensure_unique_ssh_home(user, Path::new(self.home))?;
        Ok(account)
    }
}

struct Review<'a> {
    goal: Goal<'a>,
    expected: &'a FileObservation,
    account: Option<&'a UserObservation>,
}

impl<'a> Review<'a> {
    fn new(action: &'a Action) -> Result<Self, BackendError> {
        let goal = Goal::new(&action.kind)?;
        let Some((Precondition::File { path, expected }, tail)) =
            action.preconditions.split_first()
        else {
            return Err(BackendError("SSH 授权动作缺少对应文件的前置条件".into()));
        };
        let account = match tail {
            [] => None,
            [
                Precondition::UserAccount {
                    user,
                    expected: Some(account),
                },
            ] if *user == goal.declaration.user => Some(account),
            _ => return Err(BackendError("SSH 授权动作的账户前置条件无效".into())),
        };
        if Path::new(path) != authorized_keys(Path::new(goal.home)) {
            return Err(BackendError("SSH 授权动作的文件前置条件路径不符".into()));
        }
        Ok(Self {
            goal,
            expected,
            account,
        })
    }

    fn readiness(
        &self,
        current: &Snapshot,
        account: &SshUser,
    ) -> Result<ActionReadiness, BackendError> {
        if self
            .account
            .is_some_and(|expected| *expected != user_observation(account, self.goal.home.into()))
        {
            return Ok(ActionReadiness::Conflict(
                "SSH 授权账户的 UID、GID 或 home 在审阅后改变".into(),
            ));
        }
        if current.satisfied(self.goal.managed_digest, Some(account.uid))? {
            return Ok(ActionReadiness::Satisfied);
        }
        if current.observation() != *self.expected {
            return Ok(ActionReadiness::Conflict(
                "authorized_keys 在审阅后改变，请重新规划".into(),
            ));
        }
        Ok(ActionReadiness::Ready)
    }
}

#[derive(Default)]
struct Snapshot {
    ssh: Option<TargetPath>,
    target: Option<TargetPath>,
    stat: Option<Stat>,
    bytes: Vec<u8>,
}

impl Snapshot {
    fn open(home: &str, uid: Option<u32>) -> Result<Self, BackendError> {
        let home = Path::new(home);
        let result = Self::read(home, uid);
        result.map_err(|error| path_error(&authorized_keys(home), error))
    }

    fn read(home: &Path, uid: Option<u32>) -> io::Result<Self> {
        let Some(home_path) = TargetPath::open(home)? else {
            return Ok(Self::default());
        };
        let Some(directory) = home_path.open_directory()? else {
            return Ok(Self::default());
        };
        require_safe_owner(&fstat(&directory)?, uid)?;
        let ssh = TargetPath::child_of(directory, ".ssh");
        let Some(directory) = ssh.open_directory()? else {
            return Ok(Self {
                ssh: Some(ssh),
                ..Self::default()
            });
        };
        require_safe_owner(&fstat(&directory)?, uid)?;
        let target = TargetPath::child_of(directory, "authorized_keys");
        let mut bytes = Vec::new();
        let stat = target.observe()?;
        if let Some(stat) = &stat {
            require_safe_owner(stat, uid)?;
            if stat.st_nlink != 1 {
                return Err(io::Error::other("authorized_keys 不得与其他文件共用硬链接"));
            }
            let mut file = target.read(stat)?;
            file.read_to_end(&mut bytes)?;
            if !same_revision(stat, &fstat(&file)?) {
                return Err(io::Error::other("authorized_keys 在读取期间改变"));
            }
        }
        Ok(Self {
            ssh: Some(ssh),
            target: Some(target),
            stat,
            bytes,
        })
    }

    fn observation(&self) -> FileObservation {
        match &self.stat {
            None => FileObservation::Missing,
            Some(stat) => FileObservation::Present(FileContent {
                digest: digest(&self.bytes),
                attributes: FileAttributes {
                    owner: Some(stat.st_uid.to_string()),
                    group: Some(stat.st_gid.to_string()),
                    mode: Some(format!("{:04o}", stat.st_mode & 0o7777)),
                },
            }),
        }
    }

    fn satisfied(&self, body: &ContentDigest, uid: Option<u32>) -> Result<bool, BackendError> {
        let document = Document::read(&self.bytes)?;
        if digest(document.managed) != *body || document.render(document.managed) != self.bytes {
            return Ok(false);
        }
        if document.managed.is_empty() {
            return Ok(true);
        }
        Ok(self
            .stat
            .as_ref()
            .is_some_and(|stat| uid == Some(stat.st_uid) && stat.st_mode & 0o7777 == 0o600))
    }
}

fn require_safe_owner(stat: &Stat, uid: Option<u32>) -> io::Result<()> {
    if (stat.st_uid != 0 && Some(stat.st_uid) != uid) || stat.st_mode & 0o022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "SSH 授权路径须由 root 或目标账户持有，其他用户和组不得写入",
        ));
    }
    Ok(())
}

fn overlapping_path(path: &Path) -> BackendError {
    BackendError(format!("SSH 授权文件 {path:?} 与其他路径资源冲突"))
}

fn path_error(path: &Path, error: io::Error) -> BackendError {
    BackendError(format!("无法访问 SSH 授权文件 {path:?}：{error}"))
}
