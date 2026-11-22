//! System manager unit definitions, enablement, and live state.

use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use dek_core::{
    Action, ActionKind, ContentDigest, DesiredResource, Observation, Precondition,
    ResourceCapability, ResourceId, ResourceKind, SystemdUnitResource, UnitDefinition, UnitId,
    UnitScope,
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
            attributes: ["content", "dropIns", "enabled", "running", "activates"]
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
        }
        Ok(())
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
                expected: is_running(unit)?,
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
                        "系统级单元定义或 drop-in 需要更新",
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
                            "启用系统级单元自启"
                        } else {
                            "取消系统级单元自启"
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
                        if running {
                            "启动系统级单元"
                        } else {
                            "停止系统级单元"
                        },
                        false,
                    )
                }
                _ => return Err(BackendError("无效的系统级单元观测".into())),
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
            ActionKind::ReloadManager(UnitScope::System) => Ok(()),
            _ => Err(BackendError("后端无法执行此系统级单元动作".into())),
        }
    }

    fn inspect_action(&self, action: &Action) -> Result<ActionReadiness, BackendError> {
        self.validate_action(action)?;
        let (origin, actual, satisfied) = match &action.kind {
            ActionKind::DeployUnit { unit, definition } => {
                let actual = read_definition(unit, definition)?;
                (
                    &unit.name,
                    Precondition::ResourceState {
                        resource: ResourceId::SystemdUnit(unit.clone()),
                        expected: definition_digest(&actual),
                    },
                    actual == *definition,
                )
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
                let actual = is_running(unit)?;
                (
                    &unit.name,
                    Precondition::UnitRunning {
                        unit: unit.clone(),
                        expected: actual,
                    },
                    actual == *running,
                )
            }
            ActionKind::ReloadManager(UnitScope::System) => {
                return Ok(ActionReadiness::Ready);
            }
            _ => return Err(BackendError("未知的系统级单元动作".into())),
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

    fn execute(&self, context: &ActionContext<'_>) -> Result<(), BackendError> {
        match self.inspect_action(context.action)? {
            ActionReadiness::Satisfied => return Ok(()),
            ActionReadiness::Conflict(reason) => return Err(BackendError(reason)),
            ActionReadiness::Ready => {}
        }
        match &context.action.kind {
            ActionKind::DeployUnit { unit, definition } => deploy(unit, definition, context),
            ActionKind::ReloadManager(UnitScope::System) => ctl(&["daemon-reload"]).map(|_| ()),
            ActionKind::SetUnitEnabled { unit, enabled } => {
                let action = if *enabled { "enable" } else { "disable" };
                ctl(&[action, "--no-reload", "--", &unit.name]).map(|_| ())
            }
            ActionKind::SetUnitRunning { unit, running } => {
                let action = if *running { "start" } else { "stop" };
                ctl(&[action, "--", &unit.name]).map(|_| ())
            }
            _ => Err(BackendError("未知的系统级单元动作".into())),
        }
    }

    fn verify(&self, action: &ActionKind) -> Result<bool, BackendError> {
        match action {
            ActionKind::DeployUnit { unit, definition } => {
                Ok(read_definition(unit, definition)? == *definition)
            }
            ActionKind::SetUnitEnabled { unit, enabled } => Ok(is_enabled(unit)? == *enabled),
            ActionKind::SetUnitRunning { unit, running } => Ok(is_running(unit)? == *running),
            ActionKind::ReloadManager(UnitScope::System) => {
                // The reload command is synchronous. Confirm the manager can reply afterwards.
                ctl(&["show", "--property=Version", "--value"])
                    .map(|version| !version.trim().is_empty())
            }
            _ => Err(BackendError("未知的系统级单元动作".into())),
        }
    }
}

fn unit_resource(resource: &DesiredResource) -> Result<&SystemdUnitResource, BackendError> {
    match resource {
        DesiredResource::SystemdUnit(unit) => Ok(unit),
        _ => Err(BackendError("后端收到非系统级单元资源".into())),
    }
}

fn validate_unit(unit: &UnitId) -> Result<(), BackendError> {
    let name = &unit.name;
    if unit.scope != UnitScope::System {
        return Err(BackendError("此后端只支持系统级单元".into()));
    }
    if name.is_empty()
        || name.starts_with('-')
        || !name.contains('.')
        || !name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'_' | b'.' | b'@' | b'-' | b'\\')
        })
        || Path::new(name).file_name().is_none()
    {
        return Err(BackendError(format!("无效的系统级单元名称 {name:?}")));
    }
    Ok(())
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
    let root = Path::new(UNIT_ROOT);
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
    let root = Path::new(UNIT_ROOT);
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
            Some(Uid::from_raw(0)),
            Some(Gid::from_raw(0)),
            Mode::from_bits_truncate(0o644),
        )
        .map_err(|err| file_error(path, err))?;
        staged.push((path, file, prior.as_ref()));
    }
    match SystemdBackend.inspect_action(context.action)? {
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
    let output = ctl_output(&["is-enabled", "--", &unit.name])?;
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

fn is_running(unit: &UnitId) -> Result<bool, BackendError> {
    validate_unit(unit)?;
    let output = ctl_output(&["show", "--property=LoadState,ActiveState", "--", &unit.name])?;
    let stdout = std::str::from_utf8(&output.stdout)
        .map_err(|_| BackendError("systemctl 返回无效 UTF-8".into()))?;
    let values: BTreeMap<_, _> = stdout
        .lines()
        .filter_map(|line| line.split_once('='))
        .collect();
    if values.get("LoadState") == Some(&"not-found") {
        return Ok(false);
    }
    if !output.status.success()
        || !values.contains_key("ActiveState")
        || !values.contains_key("LoadState")
    {
        return Err(ctl_error("show", &output));
    }
    Ok(values
        .get("ActiveState")
        .is_some_and(|state| matches!(*state, "active" | "reloading")))
}

fn ctl_output(args: &[&str]) -> Result<Output, BackendError> {
    Command::new("systemctl")
        .args(["--system", "--no-pager"])
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .map_err(|err| BackendError(format!("无法启动 systemctl：{err}")))
}

fn ctl(args: &[&str]) -> Result<String, BackendError> {
    let output = ctl_output(args)?;
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
