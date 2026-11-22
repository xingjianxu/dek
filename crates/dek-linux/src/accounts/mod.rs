//! Reconcile local Linux users and groups against the target account databases.

mod database;
mod nss;
mod primary;
pub(crate) mod tools;

pub(crate) use nss::{UserAccount, user_account, user_account_by_uid};

#[cfg(test)]
mod tests;

use dek_core::{
    Action, ActionKind, DesiredResource, Precondition, ResourceCapability, ResourceId,
    ResourceKind, ResourceSet, UserObservation, UserResource,
};
use dek_target::backend::{ActionContext, ActionReadiness, Backend, BackendError, PlannedAction};

use database::{Database, LocalUser, Snapshot};

pub use nss::{group_id, user_id};

pub(crate) use database::LocalUser as SshUser;

pub(crate) fn ssh_user(user: &str) -> Result<Option<SshUser>, BackendError> {
    name(user, false)?;
    Ok(Database::default().read()?.user(user, None)?.cloned())
}

pub(crate) fn ensure_unique_ssh_home(
    user: &str,
    home: &std::path::Path,
) -> Result<(), BackendError> {
    Database::default()
        .read()?
        .ensure_unique_ssh_home(user, home)
}

#[derive(Default)]
pub struct AccountBackend {
    database: Database,
}

impl Backend for AccountBackend {
    fn capabilities(&self) -> Vec<ResourceCapability> {
        vec![
            capability(
                ResourceKind::User,
                &[
                    "kind",
                    "uid",
                    "group",
                    "extraGroups",
                    "home",
                    "shell",
                    "description",
                ],
            ),
            capability(ResourceKind::Group, &["gid"]),
        ]
    }

    fn validate(&self, resource: &DesiredResource) -> Result<(), BackendError> {
        match resource {
            DesiredResource::Group(group) => {
                name(&group.name, true)?;
                numeric_id(group.gid)
            }
            DesiredResource::User(user) => {
                name(&user.name, false)?;
                numeric_id(user.uid)?;
                for group in groups(user) {
                    name(group, true)?;
                }
                for field in [&user.home, &user.shell].into_iter().flatten() {
                    if !field.starts_with('/') || !valid_field(field) {
                        return Err(BackendError("home 与 shell 需要绝对路径".into()));
                    }
                }
                if user
                    .description
                    .as_ref()
                    .is_some_and(|value| !valid_field(value))
                {
                    return Err(BackendError("账户说明含无效的字段分隔符".into()));
                }
                Ok(())
            }
            _ => Err(BackendError("需要本地用户或组资源".into())),
        }
    }

    fn resource_dependencies(
        &self,
        resource: &DesiredResource,
        managed: &ResourceSet,
    ) -> Result<Vec<ResourceId>, BackendError> {
        let mut dependencies = Vec::new();
        let id = requested_id(resource);
        for other in managed.resources.iter().map(|resource| &resource.desired) {
            if other.kind() != resource.kind() || other.id() == resource.id() {
                continue;
            }
            if let Some(other_id) = requested_id(other) {
                if id == Some(other_id) {
                    return Err(BackendError(format!(
                        "声明的数字 ID {other_id} 还被 {:?} 使用",
                        other.id()
                    )));
                }
                if id.is_none() {
                    dependencies.push(other.id());
                }
            }
        }
        if let DesiredResource::User(user) = resource {
            let current = self.database.read()?;
            for group in groups(user) {
                let id = ResourceId::Group(group.clone());
                if managed.resources.iter().any(|resource| resource.id() == id) {
                    dependencies.push(id);
                } else if current.group(group, None)?.is_none() {
                    return Err(BackendError(format!(
                        "用户 {:?} 引用的本地组 {group:?} 缺失",
                        user.name
                    )));
                }
            }
        }
        Ok(dependencies)
    }

    fn observe(&self, resource: &DesiredResource) -> Result<Vec<Precondition>, BackendError> {
        Ok(vec![observe(&self.database.read()?, resource)?])
    }

    fn plan(
        &self,
        resource: &DesiredResource,
        observed: &[Precondition],
    ) -> Result<Vec<PlannedAction>, BackendError> {
        match (resource, observed) {
            (
                DesiredResource::Group(group),
                [
                    Precondition::GroupAccount {
                        group: name,
                        expected,
                    },
                ],
            ) if name == &group.name => Ok(expected
                .is_none()
                .then(|| {
                    action(
                        ActionKind::EnsureGroup(group.clone()),
                        "本地组缺失",
                        observed[0].clone(),
                    )
                })
                .into_iter()
                .collect()),
            (
                DesiredResource::User(user),
                [
                    Precondition::UserAccount {
                        user: name,
                        expected,
                    },
                ],
            ) if name == &user.name => {
                let Some(current) = expected else {
                    return Ok(vec![action(
                        ActionKind::EnsureUser(user.clone()),
                        "本地用户缺失",
                        observed[0].clone(),
                    )]);
                };
                let mut changes = Vec::new();
                if let Some(group) = &user.group {
                    let wanted = self
                        .database
                        .read()?
                        .group(group, None)?
                        .map(|group| group.gid);
                    if wanted.is_none() || wanted != current.primary_gid {
                        changes.push(action(
                            ActionKind::SetUserPrimaryGroup {
                                user: user.name.clone(),
                                group: group.clone(),
                            },
                            "主组与声明不同",
                            user_condition(
                                &user.name,
                                Some(UserObservation {
                                    uid: current.uid,
                                    primary_gid: current.primary_gid,
                                    extra_groups: None,
                                    home: None,
                                    shell: None,
                                    description: None,
                                }),
                            ),
                        ));
                    }
                }
                let ordinary = ordinary_drift(user, current);
                if !ordinary.is_empty() {
                    let mut spec = user.clone();
                    spec.group = None;
                    let mut current = current.clone();
                    current.primary_gid = None;
                    changes.push(action(
                        ActionKind::EnsureUser(spec),
                        &ordinary.join("、"),
                        user_condition(&user.name, Some(current)),
                    ));
                }
                Ok(changes)
            }
            _ => Err(BackendError("账户观测与资源不匹配".into())),
        }
    }

    fn validate_action(&self, action: &Action) -> Result<(), BackendError> {
        match (&action.kind, action.preconditions.as_slice()) {
            (
                ActionKind::EnsureGroup(group),
                [
                    Precondition::GroupAccount {
                        group: name,
                        expected: None,
                    },
                ],
            ) if name == &group.name => {
                tools::tool("groupadd")?;
            }
            (
                ActionKind::EnsureUser(user),
                [
                    Precondition::UserAccount {
                        user: name,
                        expected,
                    },
                ],
            ) if name == &user.name && (expected.is_none() || user.group.is_none()) => {
                tools::tool(if expected.is_none() {
                    "useradd"
                } else {
                    "usermod"
                })?;
            }
            (
                ActionKind::SetUserPrimaryGroup { user, .. },
                [
                    Precondition::UserAccount {
                        user: name,
                        expected: Some(expected),
                    },
                ],
            ) if user == name && expected.primary_gid.is_some() => {}
            _ => return Err(BackendError("无效的已审阅账户动作或前置条件".into())),
        }
        self.validate(&action_resource(&action.kind)?)
    }

    fn inspect_action(&self, action: &Action) -> Result<ActionReadiness, BackendError> {
        let resource = action_resource(&action.kind)?;
        let db = self.database.read()?;
        let actual = observe(&db, &resource)?;
        if satisfied(&db, &resource, &actual)? {
            return Ok(ActionReadiness::Satisfied);
        }
        if action.preconditions.as_slice() != [actual] {
            return Ok(ActionReadiness::Conflict(format!(
                "本地账户 {:?} 的审阅前置条件已改变",
                resource.id()
            )));
        }
        ensure_groups(&db, &resource)?;
        Ok(ActionReadiness::Ready)
    }

    fn execute(&self, context: &ActionContext<'_>) -> Result<(), BackendError> {
        self.validate_action(context.action)?;
        match self.inspect_action(context.action)? {
            ActionReadiness::Satisfied => return Ok(()),
            ActionReadiness::Conflict(message) => return Err(BackendError(message)),
            ActionReadiness::Ready => {}
        }
        // Creating or modifying local account records requires a privileged target process.
        if unsafe { libc::geteuid() } != 0 {
            return Err(BackendError("修改本地账户需要目标端管理员权限".into()));
        }
        match (&context.action.kind, &context.action.preconditions[0]) {
            (ActionKind::EnsureGroup(group), _) => tools::create_group(group),
            (ActionKind::EnsureUser(user), Precondition::UserAccount { expected: None, .. }) => {
                let db = self.database.read()?;
                let gid = user
                    .group
                    .as_ref()
                    .map(|name| require_group(&db, name))
                    .transpose()?;
                tools::create_user(user, gid)
            }
            (
                ActionKind::EnsureUser(user),
                Precondition::UserAccount {
                    expected: Some(actual),
                    ..
                },
            ) => tools::modify_user(user, actual),
            (
                ActionKind::SetUserPrimaryGroup { user, group },
                Precondition::UserAccount {
                    expected: Some(actual),
                    ..
                },
            ) => primary::change(
                &self.database.etc,
                user,
                actual.uid,
                actual.primary_gid.expect("validated primary GID"),
                || {
                    let db = self.database.read()?;
                    db.user(user, Some(actual.uid))?
                        .ok_or_else(|| BackendError(format!("本地用户 {user:?} 已不存在")))?;
                    require_group(&db, group)
                },
            ),
            _ => unreachable!("validated account action"),
        }
    }

    fn verify(&self, action: &ActionKind) -> Result<bool, BackendError> {
        let resource = action_resource(action)?;
        let db = self.database.read()?;
        satisfied(&db, &resource, &observe(&db, &resource)?)
    }
}

fn capability(kind: ResourceKind, attributes: &[&str]) -> ResourceCapability {
    ResourceCapability {
        kind,
        version: 1,
        attributes: attributes.iter().map(|attr| (*attr).into()).collect(),
    }
}

fn requested_id(resource: &DesiredResource) -> Option<u32> {
    match resource {
        DesiredResource::User(user) => user.uid,
        DesiredResource::Group(group) => group.gid,
        _ => None,
    }
}

fn groups(user: &UserResource) -> impl Iterator<Item = &String> {
    user.group.iter().chain(user.extra_groups.iter().flatten())
}

fn name(value: &str, group: bool) -> Result<(), BackendError> {
    let max = if group { 32 } else { 256 };
    let body = value.strip_suffix('$').unwrap_or(value);
    if body.is_empty()
        || value.len() > max
        || value.starts_with('-')
        || matches!(value, "." | "..")
        || value.bytes().all(|byte| byte.is_ascii_digit())
        || !body
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(BackendError(format!("无效的本地账户名称 {value:?}")));
    }
    Ok(())
}

fn numeric_id(value: Option<u32>) -> Result<(), BackendError> {
    if value == Some(u32::MAX) {
        Err(BackendError("数字 ID 超出可用范围".into()))
    } else {
        Ok(())
    }
}

fn valid_field(value: &str) -> bool {
    !value.contains(['\0', '\r', '\n', ':'])
}

fn action(kind: ActionKind, reason: &str, expected: Precondition) -> PlannedAction {
    PlannedAction {
        kind,
        reason: reason.into(),
        preconditions: vec![expected],
        content_changed: false,
    }
}

fn user_condition(user: &str, expected: Option<UserObservation>) -> Precondition {
    Precondition::UserAccount {
        user: user.into(),
        expected,
    }
}

fn observe(db: &Snapshot<'_>, resource: &DesiredResource) -> Result<Precondition, BackendError> {
    match resource {
        DesiredResource::Group(group) => Ok(Precondition::GroupAccount {
            group: group.name.clone(),
            expected: db.group(&group.name, group.gid)?.map(|group| group.gid),
        }),
        DesiredResource::User(user) => Ok(user_condition(
            &user.name,
            db.user(&user.name, user.uid)?
                .map(|actual| observe_user(db, user, actual))
                .transpose()?,
        )),
        _ => Err(BackendError("需要本地账户资源".into())),
    }
}

fn observe_user(
    db: &Snapshot<'_>,
    desired: &UserResource,
    actual: &LocalUser,
) -> Result<UserObservation, BackendError> {
    let extra_groups = if let Some(groups) = &desired.extra_groups {
        let mut observed = std::collections::BTreeSet::new();
        for group in groups {
            if db
                .group(group, None)?
                .is_some_and(|entry| entry.members.contains(&desired.name))
            {
                observed.insert(group.clone());
            }
        }
        Some(observed)
    } else {
        None
    };
    Ok(UserObservation {
        uid: actual.uid,
        primary_gid: desired.group.as_ref().map(|_| actual.gid),
        extra_groups,
        home: desired.home.as_ref().map(|_| actual.home.clone()),
        shell: desired.shell.as_ref().map(|_| actual.shell.clone()),
        description: desired
            .description
            .as_ref()
            .map(|_| actual.description.clone()),
    })
}

fn ordinary_drift(user: &UserResource, actual: &UserObservation) -> Vec<&'static str> {
    let mut changes = Vec::new();
    for (different, reason) in [
        (user.home != actual.home, "home 需修正"),
        (user.shell != actual.shell, "shell 需修正"),
        (user.description != actual.description, "说明需修正"),
        (
            user.extra_groups.as_ref().is_some_and(|wanted| {
                wanted.iter().any(|group| {
                    !actual
                        .extra_groups
                        .as_ref()
                        .is_some_and(|got| got.contains(group))
                })
            }),
            "补充附加组",
        ),
    ] {
        if different {
            changes.push(reason);
        }
    }
    changes
}

fn action_resource(action: &ActionKind) -> Result<DesiredResource, BackendError> {
    match action {
        ActionKind::EnsureGroup(group) => Ok(DesiredResource::Group(group.clone())),
        ActionKind::EnsureUser(user) => Ok(DesiredResource::User(user.clone())),
        ActionKind::SetUserPrimaryGroup { user, group } => {
            Ok(DesiredResource::User(UserResource {
                name: user.clone(),
                group: Some(group.clone()),
                uid: None,
                kind: None,
                extra_groups: None,
                home: None,
                shell: None,
                description: None,
            }))
        }
        _ => Err(BackendError("需要本地账户动作".into())),
    }
}

fn satisfied(
    db: &Snapshot<'_>,
    resource: &DesiredResource,
    actual: &Precondition,
) -> Result<bool, BackendError> {
    match (resource, actual) {
        (DesiredResource::Group(_), Precondition::GroupAccount { expected, .. }) => {
            Ok(expected.is_some())
        }
        (
            DesiredResource::User(user),
            Precondition::UserAccount {
                expected: Some(actual),
                ..
            },
        ) => Ok(ordinary_drift(user, actual).is_empty()
            && match &user.group {
                Some(group) => db
                    .group(group, None)?
                    .is_some_and(|group| Some(group.gid) == actual.primary_gid),
                None => true,
            }),
        (DesiredResource::User(_), Precondition::UserAccount { expected: None, .. }) => Ok(false),
        _ => Err(BackendError("账户观测与动作不匹配".into())),
    }
}

fn ensure_groups(db: &Snapshot<'_>, resource: &DesiredResource) -> Result<(), BackendError> {
    if let DesiredResource::User(user) = resource {
        for group in groups(user) {
            require_group(db, group)?;
        }
    }
    Ok(())
}

fn require_group(db: &Snapshot<'_>, group: &str) -> Result<u32, BackendError> {
    db.group(group, None)?
        .map(|group| group.gid)
        .ok_or_else(|| BackendError(format!("本地依赖组 {group:?} 尚未可用")))
}
