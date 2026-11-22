//! System and account-scoped unit definitions, enablement, and live state.

mod user;

use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use dek_core::{
    Action, ActionKind, ContentDigest, DesiredResource, Observation, Precondition,
    ResourceCapability, ResourceId, ResourceKind, ResourceSet, SystemdUnitResource, UnitDefinition,
    UnitId, UnitScope,
};
use dek_target::backend::{ActionContext, ActionReadiness, Backend, BackendError, PlannedAction};
use rustix::{
    fs::{Mode, Stat, fstat},
    process::{Gid, Uid},
};
use sha2::{Digest, Sha256};

use crate::filesystem::atomic_file::{StagedFile, TargetPath, same_revision};

pub struct SystemdBackend;

impl Backend for SystemdBackend {
    fn capabilities(&self) -> Vec<ResourceCapability> {
        vec![ResourceCapability {
            kind: ResourceKind::SystemdUnit,
            version: 1,
            attributes: [
                "content",
                "dropIns",
                "enabled",
                "running",
                "activates",
                "user",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        }]
    }

    fn validate(&self, resource: &DesiredResource) -> Result<(), BackendError> {
        let resource = unit_resource(resource)?;
        validate_unit(&resource.unit)?;
        validate_definition(&resource.definition)?;
        if let Some(service) = &resource.activates {
            validate_unit(service)?;
            if service.scope != resource.unit.scope {
                return Err(BackendError(
                    "timer 与被触发的服务须属于同一 manager".into(),
                ));
            }
        }
        Ok(())
    }

    fn resource_dependencies(
        &self,
        resource: &DesiredResource,
        managed: &ResourceSet,
    ) -> Result<Vec<ResourceId>, BackendError> {
        let UnitScope::User { user: name } = &unit_resource(resource)?.unit.scope else {
            return Ok(Vec::new());
        };
        let id = ResourceId::User(name.clone());
        if managed.resources.iter().any(|resource| resource.id() == id) {
            return Ok(vec![id]);
        }
        if user::resolve(name)?.is_none() {
            return Err(BackendError(format!(
                "用户单元需要已存在或同计划声明的账户 {name:?}"
            )));
        }
        Ok(Vec::new())
    }

    fn observe(&self, resource: &DesiredResource) -> Result<Vec<Precondition>, BackendError> {
        let resource = unit_resource(resource)?;
        let unit = &resource.unit;
        let mut conditions = Vec::new();
        if declares_definition(&resource.definition) {
            let definition = read_definition(unit, &resource.definition)?;
            conditions.push(Precondition::ResourceState {
                resource: ResourceId::SystemdUnit(unit.clone()),
                expected: definition_digest(&definition),
            });
        }
        if resource.enabled.is_some() {
            conditions.push(Precondition::UnitEnabled {
                unit: unit.clone(),
                expected: is_enabled(unit)?,
            });
        }
        if resource.running.is_some() {
            conditions.push(Precondition::UnitRunning {
                unit: unit.clone(),
                expected: is_running(unit)?.unwrap_or(false),
            });
        }
        Ok(conditions)
    }

    fn plan(
        &self,
        resource: &DesiredResource,
        observed: &[Precondition],
    ) -> Result<Vec<PlannedAction>, BackendError> {
        let resource = unit_resource(resource)?;
        let unit = &resource.unit;
        let mut actions = Vec::new();
        for condition in observed {
            let (kind, reason, content_changed) = match condition {
                Precondition::ResourceState {
                    resource: ResourceId::SystemdUnit(current),
                    expected,
                } if current == unit => {
                    if *expected == definition_digest(&resource.definition) {
                        continue;
                    }
                    (
                        ActionKind::DeployUnit {
                            unit: unit.clone(),
                            definition: resource.definition.clone(),
                        },
                        "单元定义或 drop-in 需要部署",
                        true,
                    )
                }
                Precondition::UnitEnabled {
                    unit: current,
                    expected,
                } if current == unit => {
                    let Some(enabled) = resource.enabled else {
                        continue;
                    };
                    if enabled == *expected {
                        continue;
                    }
                    (
                        ActionKind::SetUnitEnabled {
                            unit: unit.clone(),
                            enabled,
                        },
                        if enabled {
                            "启用单元自启"
                        } else {
                            "取消单元自启"
                        },
                        false,
                    )
                }
                Precondition::UnitRunning {
                    unit: current,
                    expected,
                } if current == unit => {
                    let Some(running) = resource.running else {
                        continue;
                    };
                    if running == *expected {
                        continue;
                    }
                    (
                        ActionKind::SetUnitRunning {
                            unit: unit.clone(),
                            running,
                        },
                        if running && !manager_online(&unit.scope)? {
                            "单元待激活，启动需要在线的用户 manager"
                        } else if running {
                            "启动单元"
                        } else {
                            "停止单元"
                        },
                        false,
                    )
                }
                _ => return Err(BackendError("无效的单元观测".into())),
            };
            actions.push(PlannedAction {
                kind,
                reason: reason.into(),
                preconditions: vec![condition.clone()],
                content_changed,
            });
        }
        Ok(actions)
    }

    fn validate_action(&self, action: &Action) -> Result<(), BackendError> {
        match &action.kind {
            ActionKind::DeployUnit { unit, definition } => {
                validate_unit(unit)?;
                validate_definition(definition)
            }
            ActionKind::SetUnitEnabled { unit, .. } | ActionKind::SetUnitRunning { unit, .. } => {
                validate_unit(unit)
            }
            ActionKind::ReloadManager(scope) => validate_scope(scope),
            _ => Err(BackendError("后端无法执行此类单元动作".into())),
        }
    }

    fn inspect_action(&self, action: &Action) -> Result<ActionReadiness, BackendError> {
        self.validate_action(action)?;
        let (origin, actual, satisfied) = match &action.kind {
            ActionKind::DeployUnit { unit, definition } => {
                let actual = read_definition(unit, definition)?;
                return Ok(inspect_definition(action, unit, definition, &actual));
            }
            ActionKind::SetUnitEnabled { unit, enabled } => {
                let actual = is_enabled(unit)?;
                (
                    &unit.name,
                    Precondition::UnitEnabled {
                        unit: unit.clone(),
                        expected: actual,
                    },
                    actual == *enabled,
                )
            }
            ActionKind::SetUnitRunning { unit, running } => {
                let actual = is_running(unit)?.unwrap_or(false);
                (
                    &unit.name,
                    Precondition::UnitRunning {
                        unit: unit.clone(),
                        expected: actual,
                    },
                    actual == *running,
                )
            }
            ActionKind::ReloadManager(_) => return Ok(ActionReadiness::Ready),
            _ => return Err(BackendError("未知的单元动作".into())),
        };
        if satisfied {
            Ok(ActionReadiness::Satisfied)
        } else if action.preconditions == [actual] {
            Ok(ActionReadiness::Ready)
        } else {
            Ok(ActionReadiness::Conflict(format!(
                "单元 {origin:?} 在审阅后发生变化"
            )))
        }
    }

    fn deferred_reason(&self, action: &Action) -> Result<Option<String>, BackendError> {
        let scope = match &action.kind {
            ActionKind::ReloadManager(scope) => scope,
            ActionKind::SetUnitRunning {
                unit,
                running: true,
            } => &unit.scope,
            _ => return Ok(None),
        };
        if let UnitScope::User { user } = scope
            && !manager_online(scope)?
        {
            return Ok(Some(format!(
                "账户 {user:?} 的用户 manager 离线，动作待激活"
            )));
        }
        Ok(None)
    }

    fn execute(&self, context: &ActionContext<'_>) -> Result<(), BackendError> {
        if let Some(reason) = self.deferred_reason(context.action)? {
            return Err(BackendError(reason));
        }
        match self.inspect_action(context.action)? {
            ActionReadiness::Satisfied => return Ok(()),
            ActionReadiness::Conflict(reason) => return Err(BackendError(reason)),
            ActionReadiness::Ready => {}
        }
        match &context.action.kind {
            ActionKind::DeployUnit { unit, definition } => deploy(unit, definition, context),
            ActionKind::ReloadManager(scope) => ctl(scope, false, &["daemon-reload"]).map(|_| ()),
            ActionKind::SetUnitEnabled { unit, enabled } => {
                let action = if *enabled { "enable" } else { "disable" };
                ctl(
                    &unit.scope,
                    true,
                    &[action, "--no-reload", "--", &unit.name],
                )
                .map(|_| ())
            }
            ActionKind::SetUnitRunning { unit, running } => {
                let action = if *running { "start" } else { "stop" };
                ctl(&unit.scope, false, &[action, "--", &unit.name]).map(|_| ())
            }
            _ => Err(BackendError("未知的单元动作".into())),
        }
    }

    fn verify(&self, action: &ActionKind) -> Result<bool, BackendError> {
        match action {
            ActionKind::DeployUnit { unit, definition } => {
                Ok(read_definition(unit, definition)? == *definition)
            }
            ActionKind::SetUnitEnabled { unit, enabled } => Ok(is_enabled(unit)? == *enabled),
            ActionKind::SetUnitRunning { unit, running } => {
                Ok(is_running(unit)?.unwrap_or(false) == *running)
            }
            ActionKind::ReloadManager(scope) => {
                // The reload command is synchronous. Confirm the manager can reply afterwards.
                ctl(scope, false, &["show", "--property=Version", "--value"])
                    .map(|version| !version.trim().is_empty())
            }
            _ => Err(BackendError("未知的单元动作".into())),
        }
    }
}

fn unit_resource(resource: &DesiredResource) -> Result<&SystemdUnitResource, BackendError> {
    match resource {
        DesiredResource::SystemdUnit(unit) => Ok(unit),
        _ => Err(BackendError("后端收到非单元资源".into())),
    }
}

fn validate_unit(unit: &UnitId) -> Result<(), BackendError> {
    let name = &unit.name;
    validate_scope(&unit.scope)?;
    if name.is_empty()
        || name.starts_with('-')
        || !name.contains('.')
        || !name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'_' | b'.' | b'@' | b'-' | b'\\')
        })
        || Path::new(name).file_name().is_none()
    {
        return Err(BackendError(format!("无效的单元名称 {name:?}")));
    }
    Ok(())
}

fn validate_scope(scope: &UnitScope) -> Result<(), BackendError> {
    match scope {
        UnitScope::System => Ok(()),
        UnitScope::User { user }
            if !user.is_empty()
                && !user.starts_with('-')
                && !matches!(user.as_str(), "." | "..")
                && user.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')
                }) =>
        {
            Ok(())
        }
        UnitScope::User { user } => Err(BackendError(format!("无效的用户名称 {user:?}"))),
        UnitScope::UserGlobal => Err(BackendError("此后端需要指定用户级单元的账户".into())),
    }
}

fn validate_definition(definition: &UnitDefinition) -> Result<(), BackendError> {
    for name in definition.drop_ins.keys() {
        if !name.ends_with(".conf")
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
        {
            return Err(BackendError(format!("无效的 drop-in 文件名 {name:?}")));
        }
    }
    Ok(())
}

fn declares_definition(definition: &UnitDefinition) -> bool {
    definition.content.is_some() || !definition.drop_ins.is_empty()
}

fn definition_digest(definition: &UnitDefinition) -> Observation {
    let mut hash = Sha256::new();
    let mut add = |bytes: &[u8]| {
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(bytes);
    };
    add(b"systemd-definition-v1");
    add(definition
        .content
        .as_ref()
        .map_or(b"".as_slice(), |digest| digest.0.as_bytes()));
    for (name, digest) in &definition.drop_ins {
        add(name.as_bytes());
        add(digest.0.as_bytes());
    }
    Observation::Digest(format!("sha256:{:x}", hash.finalize()))
}

const UNIT_ROOT: &str = "/etc/systemd/system";

fn read_definition(
    unit: &UnitId,
    desired: &UnitDefinition,
) -> Result<UnitDefinition, BackendError> {
    validate_unit(unit)?;
    validate_definition(desired)?;
    match &unit.scope {
        UnitScope::System => read_definition_at(Path::new(UNIT_ROOT), unit, desired),
        UnitScope::User { user: name } => {
            let Some(account) = user::resolve(name)? else {
                return Ok(UnitDefinition::default());
            };
            user::with_permissions(&account, || {
                read_definition_at(&user::config_dir(&account), unit, desired)
            })
        }
        UnitScope::UserGlobal => unreachable!("validated unit scope"),
    }
}

fn read_definition_at(
    root: &Path,
    unit: &UnitId,
    desired: &UnitDefinition,
) -> Result<UnitDefinition, BackendError> {
    let content = if desired.content.is_some() {
        file_digest(&root.join(&unit.name))?
    } else {
        None
    };
    let mut drop_ins = BTreeMap::new();
    let drop_root = root.join(format!("{}.d", unit.name));
    for name in desired.drop_ins.keys() {
        if let Some(digest) = file_digest(&drop_root.join(name))? {
            drop_ins.insert(name.clone(), digest);
        }
    }
    Ok(UnitDefinition { content, drop_ins })
}

fn inspect_definition(
    action: &Action,
    unit: &UnitId,
    desired: &UnitDefinition,
    observed: &UnitDefinition,
) -> ActionReadiness {
    let condition = Precondition::ResourceState {
        resource: ResourceId::SystemdUnit(unit.clone()),
        expected: definition_digest(observed),
    };
    if desired == observed {
        ActionReadiness::Satisfied
    } else if action.preconditions == [condition] {
        ActionReadiness::Ready
    } else {
        ActionReadiness::Conflict(format!("单元 {:?} 在审阅后发生变化", unit.name))
    }
}

fn file_digest(path: &Path) -> Result<Option<ContentDigest>, BackendError> {
    let problem = |err| file_error(path, err);
    let Some(target) = TargetPath::open(path).map_err(problem)? else {
        return Ok(None);
    };
    let Some(stat) = target.observe().map_err(problem)? else {
        return Ok(None);
    };
    read_digest(&target, &stat).map_err(problem).map(Some)
}

fn read_digest(target: &TargetPath, stat: &Stat) -> io::Result<ContentDigest> {
    let mut file = target.read(stat)?;
    let mut hash = Sha256::new();
    io::copy(&mut file, &mut hash)?;
    if !same_revision(stat, &fstat(&file)?) {
        return Err(io::Error::other("unit definition changed during read"));
    }
    Ok(ContentDigest(format!("sha256:{:x}", hash.finalize())))
}

fn file_error(path: &Path, error: io::Error) -> BackendError {
    BackendError(format!("无法访问单元文件 {path:?}：{error}"))
}

fn deploy(
    unit: &UnitId,
    definition: &UnitDefinition,
    context: &ActionContext<'_>,
) -> Result<(), BackendError> {
    match &unit.scope {
        UnitScope::System => deploy_at(Path::new(UNIT_ROOT), unit, definition, context),
        UnitScope::User { user: name } => {
            let account = user::resolve(name)?
                .ok_or_else(|| BackendError(format!("缺少用户单元所属账户 {name:?}")))?;
            required_target(&account.home)?
                .create_home(Uid::from_raw(account.uid), Gid::from_raw(account.gid))
                .map_err(|err| file_error(&account.home, err))?;
            user::with_permissions(&account, || {
                let root = user::config_dir(&account);
                for directory in [
                    account.home.join(".config"),
                    account.home.join(".config/systemd"),
                    root.clone(),
                ] {
                    required_target(&directory)?
                        .ensure_directory(Mode::from_bits_truncate(0o755))
                        .map_err(|err| file_error(&directory, err))?;
                }
                deploy_at(&root, unit, definition, context)
            })
        }
        UnitScope::UserGlobal => unreachable!("validated unit scope"),
    }
}

fn deploy_at(
    root: &Path,
    unit: &UnitId,
    definition: &UnitDefinition,
    context: &ActionContext<'_>,
) -> Result<(), BackendError> {
    let mut candidates = Vec::<(PathBuf, &ContentDigest)>::new();
    if let Some(content) = &definition.content {
        candidates.push((root.join(&unit.name), content));
    }
    let drop_root = root.join(format!("{}.d", unit.name));
    if !definition.drop_ins.is_empty() {
        required_target(&drop_root)?
            .ensure_directory(Mode::from_bits_truncate(0o755))
            .map_err(|err| file_error(&drop_root, err))?;
        for (name, digest) in &definition.drop_ins {
            candidates.push((drop_root.join(name), digest));
        }
    }

    let mut files = Vec::new();
    for (path, expected) in candidates {
        let target = required_target(&path)?;
        let problem = |err| file_error(&path, err);
        let prior = target.observe().map_err(problem)?;
        if let Some(stat) = &prior
            && read_digest(&target, stat).map_err(problem)? == *expected
        {
            continue;
        }
        files.push((path, target, prior, expected));
    }

    // Stage and check the entire unit definition before replacing its first member.
    let mut staged = Vec::new();
    for (path, target, prior, expected) in &files {
        let file = StagedFile::new(
            target,
            context.content(expected)?,
            (unit.scope == UnitScope::System).then(|| Uid::from_raw(0)),
            (unit.scope == UnitScope::System).then(|| Gid::from_raw(0)),
            Mode::from_bits_truncate(0o644),
        )
        .map_err(|err| file_error(path, err))?;
        staged.push((path, file, prior.as_ref()));
    }
    let observed = read_definition_at(root, unit, definition)?;
    match inspect_definition(context.action, unit, definition, &observed) {
        ActionReadiness::Satisfied => return Ok(()),
        ActionReadiness::Conflict(reason) => return Err(BackendError(reason)),
        ActionReadiness::Ready => {}
    }
    for (path, file, prior) in staged {
        file.publish(prior).map_err(|err| file_error(path, err))?;
    }
    Ok(())
}

fn required_target(path: &Path) -> Result<TargetPath, BackendError> {
    TargetPath::open(path)
        .map_err(|err| file_error(path, err))?
        .ok_or_else(|| BackendError(format!("单元文件父目录不存在 {path:?}")))
}

fn is_enabled(unit: &UnitId) -> Result<bool, BackendError> {
    validate_unit(unit)?;
    let Some(output) = ctl_output(&unit.scope, true, &["is-enabled", "--", &unit.name])? else {
        return Ok(false);
    };
    let state = std::str::from_utf8(&output.stdout)
        .map_err(|_| BackendError("systemctl 返回无效 UTF-8".into()))?;
    match state.trim() {
        "enabled" if output.status.success() => Ok(true),
        "disabled" | "enabled-runtime" | "static" | "masked" | "masked-runtime" | "linked"
        | "linked-runtime" | "alias" | "indirect" | "generated" | "transient" | "not-found"
        | "bad" => Ok(false),
        _ => Err(ctl_error("is-enabled", &output)),
    }
}

fn is_running(unit: &UnitId) -> Result<Option<bool>, BackendError> {
    validate_unit(unit)?;
    let Some(output) = ctl_output(
        &unit.scope,
        false,
        &["show", "--property=LoadState,ActiveState", "--", &unit.name],
    )?
    else {
        return Ok(None);
    };
    let stdout = std::str::from_utf8(&output.stdout)
        .map_err(|_| BackendError("systemctl 返回无效 UTF-8".into()))?;
    let values: BTreeMap<_, _> = stdout
        .lines()
        .filter_map(|line| line.split_once('='))
        .collect();
    if values.get("LoadState") == Some(&"not-found") {
        return Ok(Some(false));
    }
    if !output.status.success()
        || !values.contains_key("ActiveState")
        || !values.contains_key("LoadState")
    {
        return Err(ctl_error("show", &output));
    }
    Ok(Some(values.get("ActiveState").is_some_and(|state| {
        matches!(*state, "active" | "reloading")
    })))
}

fn manager_online(scope: &UnitScope) -> Result<bool, BackendError> {
    match scope {
        UnitScope::System => Ok(true),
        UnitScope::User { user: name } => match user::resolve(name)? {
            Some(account) => user::online(&account),
            None => Ok(false),
        },
        UnitScope::UserGlobal => unreachable!("validated unit scope"),
    }
}

fn ctl_output(
    scope: &UnitScope,
    disk_only: bool,
    args: &[&str],
) -> Result<Option<Output>, BackendError> {
    match scope {
        UnitScope::System => Command::new("systemctl")
            .args(["--system", "--no-pager"])
            .args(args)
            .env("LC_ALL", "C")
            .output()
            .map(Some)
            .map_err(|err| BackendError(format!("无法启动 systemctl：{err}"))),
        UnitScope::User { user: name } => {
            let Some(account) = user::resolve(name)? else {
                return Ok(None);
            };
            if !disk_only && !user::online(&account)? {
                return Ok(None);
            }
            user::command(&account, disk_only, args).map(Some)
        }
        UnitScope::UserGlobal => unreachable!("validated unit scope"),
    }
}

fn ctl(scope: &UnitScope, disk_only: bool, args: &[&str]) -> Result<String, BackendError> {
    let output = ctl_output(scope, disk_only, args)?
        .ok_or_else(|| BackendError(format!("systemctl {} 无法访问 {scope:?}", args[0])))?;
    if !output.status.success() {
        return Err(ctl_error(args[0], &output));
    }
    String::from_utf8(output.stdout).map_err(|_| BackendError("systemctl 返回无效 UTF-8".into()))
}

fn ctl_error(command: &str, output: &Output) -> BackendError {
    BackendError(format!(
        "systemctl {command} 失败（{}）{}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}
