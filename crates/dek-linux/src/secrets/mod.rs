//! Plan and deploy age resources with target-local verification markers.

mod apply;
mod marker;
mod path;

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use dek_core::{
    Action, ActionKind, AgeSecretObservation, AgeSecretResource, AgeSecretRevision,
    AgeSecretVerification, DesiredResource, Precondition, ResourceCapability, ResourceId,
    ResourceKind, ResourceSet,
};
use dek_target::backend::{ActionContext, ActionReadiness, Backend, BackendError, PlannedAction};

use crate::accounts;
use path::CheckedPath;

pub use marker::verification_marker;

pub struct SecretBackend {
    state_root: PathBuf,
}

impl SecretBackend {
    pub fn new(state_root: PathBuf) -> Self {
        Self { state_root }
    }

    fn observation(
        &self,
        secret: &AgeSecretResource,
    ) -> Result<AgeSecretObservation, BackendError> {
        self.observe_path(secret, &CheckedPath::inspect(&secret.path)?)
    }

    fn observe_path(
        &self,
        secret: &AgeSecretResource,
        output: &CheckedPath,
    ) -> Result<AgeSecretObservation, BackendError> {
        let Some(file) = output.file.as_ref() else {
            return Ok(AgeSecretObservation::Missing);
        };
        let verification = marker::observe(&self.state_root, secret, &output.normalized, file)?;
        Ok(AgeSecretObservation::Present {
            verification,
            revision: Some(AgeSecretRevision {
                device: file.stat.st_dev,
                inode: file.stat.st_ino,
                changed_seconds: file.stat.st_ctime,
                changed_nanoseconds: file.stat.st_ctime_nsec as i64,
            }),
            uid: secret.attributes.owner.as_ref().map(|_| file.stat.st_uid),
            gid: secret.attributes.group.as_ref().map(|_| file.stat.st_gid),
            mode: file.stat.st_mode & 0o7777,
        })
    }
}

impl Backend for SecretBackend {
    fn capabilities(&self) -> Vec<ResourceCapability> {
        vec![ResourceCapability {
            kind: ResourceKind::AgeSecret,
            version: 1,
            attributes: ["owner", "group", "mode"].map(str::to_owned).into(),
        }]
    }

    fn validate(&self, resource: &DesiredResource) -> Result<(), BackendError> {
        let secret = age_secret(resource)?;
        let digest = secret
            .ciphertext
            .0
            .strip_prefix("sha256:")
            .unwrap_or_default();
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
        {
            return Err(BackendError("需要有效的密文 SHA-256 摘要".into()));
        }
        if secret.identity_paths.is_empty() {
            return Err(BackendError(
                "需要声明至少一个目标机 age identity 路径".into(),
            ));
        }
        mode(secret)?;
        for account in [&secret.attributes.owner, &secret.attributes.group]
            .into_iter()
            .flatten()
        {
            if account.is_empty() || account.contains('\0') {
                return Err(BackendError("加密文件的属主或属组名称无效".into()));
            }
        }
        let output = CheckedPath::inspect(&secret.path)?;
        if output.normalized.starts_with(&self.state_root) {
            return Err(BackendError(format!(
                "加密文件落点不得写入目标状态目录 {:?}",
                secret.path
            )));
        }
        for raw in &secret.identity_paths {
            let identity = CheckedPath::identity(raw)?;
            if output.aliases(&identity) {
                return Err(identity_collision(&secret.path, raw));
            }
        }
        Ok(())
    }

    fn validate_managed_paths(
        &self,
        resource: &DesiredResource,
        managed: &[ResourceId],
    ) -> Result<(), BackendError> {
        let secret = age_secret(resource)?;
        let output = CheckedPath::inspect(&secret.path)?;
        if let Some(path) = conflicting_path(&output, managed, Some(&secret.path))? {
            return Err(BackendError(format!(
                "受管路径 {path:?} 与加密文件落点 {:?} 冲突",
                secret.path
            )));
        }
        for identity_path in &secret.identity_paths {
            let identity = CheckedPath::identity(identity_path)?;
            if let Some(path) = conflicting_path(&identity, managed, None)? {
                return Err(identity_collision(path, identity_path));
            }
        }
        Ok(())
    }

    fn resource_dependencies(
        &self,
        resource: &DesiredResource,
        managed: &ResourceSet,
    ) -> Result<Vec<ResourceId>, BackendError> {
        let secret = age_secret(resource)?;
        let mut dependencies = Vec::new();
        for (name, id, lookup) in [
            (
                secret.attributes.owner.as_ref(),
                ResourceId::User as fn(String) -> ResourceId,
                accounts::user_id as fn(&str) -> Result<Option<u32>, BackendError>,
            ),
            (
                secret.attributes.group.as_ref(),
                ResourceId::Group,
                accounts::group_id,
            ),
        ] {
            if let Some(name) = name {
                let identity = id(name.clone());
                if managed.resources.iter().any(|entry| entry.id() == identity) {
                    dependencies.push(identity);
                } else if lookup(name)?.is_none() {
                    return Err(BackendError(format!(
                        "加密文件引用的账户或组缺失 {identity:?}"
                    )));
                }
            }
        }
        let path = path::normalized_absolute(&secret.path)?;
        for resource in &managed.resources {
            if let DesiredResource::Directory(parent) = &resource.desired
                && path != Path::new(&parent.path)
                && path.starts_with(&parent.path)
            {
                dependencies.push(resource.id());
            }
        }
        Ok(dependencies)
    }

    fn observe(&self, resource: &DesiredResource) -> Result<Vec<Precondition>, BackendError> {
        let secret = age_secret(resource)?;
        Ok(vec![Precondition::AgeSecret {
            path: secret.path.clone(),
            expected: self.observation(secret)?,
        }])
    }

    fn plan(
        &self,
        resource: &DesiredResource,
        observed: &[Precondition],
    ) -> Result<Vec<PlannedAction>, BackendError> {
        let secret = age_secret(resource)?;
        let [Precondition::AgeSecret { path, expected }] = observed else {
            return Err(BackendError("需要目标端加密文件观测".into()));
        };
        if path != &secret.path {
            return Err(BackendError("加密文件观测路径不匹配".into()));
        }

        let mut reasons = Vec::new();
        let deploy = match expected {
            AgeSecretObservation::Missing => {
                reasons.push("目标加密文件缺失");
                true
            }
            AgeSecretObservation::Present { verification, .. } => match verification {
                AgeSecretVerification::Matched => false,
                AgeSecretVerification::Drifted => {
                    reasons.push("已部署内容发生漂移");
                    true
                }
                AgeSecretVerification::Unverified => {
                    reasons.push("缺少可用验证标记，应用时核对");
                    true
                }
                AgeSecretVerification::InputsChanged => {
                    reasons.push("密文摘要或 identity 路径改变，应用时核对");
                    true
                }
            },
        };
        reasons.extend(attribute_drift(secret, expected)?);
        if reasons.is_empty() {
            return Ok(Vec::new());
        }
        let kind = if deploy {
            ActionKind::DeployAgeSecret(secret.clone())
        } else {
            ActionKind::SetAgeSecretAttributes(secret.clone())
        };
        Ok(vec![PlannedAction {
            kind,
            reason: reasons.join("、"),
            preconditions: observed.to_vec(),
            content_changed: deploy,
        }])
    }

    fn validate_action(&self, action: &Action) -> Result<(), BackendError> {
        let (secret, _) = reviewed(action)?;
        self.validate(&DesiredResource::AgeSecret(secret.clone()))
    }

    fn inspect_action(&self, action: &Action) -> Result<ActionReadiness, BackendError> {
        let (secret, expected) = reviewed(action)?;
        self.validate_action(action)?;
        readiness(secret, expected, &self.observation(secret)?)
    }

    fn execute(&self, context: &ActionContext<'_>) -> Result<(), BackendError> {
        self.execute_changed(context).map(drop)
    }

    fn execute_changed(&self, context: &ActionContext<'_>) -> Result<bool, BackendError> {
        let (secret, expected) = reviewed(context.action)?;
        self.validate_action(context.action)?;
        let output = CheckedPath::inspect(&secret.path)?;
        let observed = self.observe_path(secret, &output)?;
        match readiness(secret, expected, &observed)? {
            ActionReadiness::Satisfied => Ok(false),
            ActionReadiness::Conflict(reason) => Err(BackendError(reason)),
            ActionReadiness::Ready => apply::apply(&self.state_root, secret, &output, context),
        }
    }

    fn verify(&self, action: &ActionKind) -> Result<bool, BackendError> {
        let (ActionKind::DeployAgeSecret(secret) | ActionKind::SetAgeSecretAttributes(secret)) =
            action
        else {
            return Err(BackendError("需要加密文件动作".into()));
        };
        self.validate(&DesiredResource::AgeSecret(secret.clone()))?;
        let observed = self.observation(secret)?;
        Ok(matches!(
            observed,
            AgeSecretObservation::Present {
                verification: AgeSecretVerification::Matched,
                ..
            }
        ) && attribute_drift(secret, &observed)?.is_empty())
    }
}

fn reviewed(action: &Action) -> Result<(&AgeSecretResource, &AgeSecretObservation), BackendError> {
    let (ActionKind::DeployAgeSecret(secret) | ActionKind::SetAgeSecretAttributes(secret)) =
        &action.kind
    else {
        return Err(BackendError("需要加密文件动作".into()));
    };
    match action.preconditions.as_slice() {
        [Precondition::AgeSecret { path, expected }] if path == &secret.path => {
            Ok((secret, expected))
        }
        _ => Err(BackendError("加密文件动作缺少对应路径的前置条件".into())),
    }
}

fn readiness(
    secret: &AgeSecretResource,
    expected: &AgeSecretObservation,
    actual: &AgeSecretObservation,
) -> Result<ActionReadiness, BackendError> {
    if matches!(
        actual,
        AgeSecretObservation::Present {
            verification: AgeSecretVerification::Matched,
            ..
        }
    ) && attribute_drift(secret, actual)?.is_empty()
    {
        return Ok(ActionReadiness::Satisfied);
    }
    if actual == expected {
        Ok(ActionReadiness::Ready)
    } else {
        Ok(ActionReadiness::Conflict(format!(
            "加密文件在审阅后发生变化 {:?}",
            secret.path
        )))
    }
}

fn conflicting_path<'a>(
    protected: &CheckedPath,
    managed: &'a [ResourceId],
    own_path: Option<&str>,
) -> Result<Option<&'a str>, BackendError> {
    for resource in managed {
        let ResourceId::Path(path) = resource else {
            continue;
        };
        if own_path == Some(path) {
            // The planner already rejects duplicate declarations of the same path.
            continue;
        }
        if Path::new(path) == protected.normalized {
            return Ok(Some(path));
        }
        let Some(file) = &protected.file else {
            continue;
        };
        match fs::metadata(path) {
            Ok(metadata) if file.matches_metadata(&metadata) => return Ok(Some(path)),
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(BackendError(format!("检查受管路径 {path:?}：{err}"))),
        }
    }
    Ok(None)
}

fn age_secret(resource: &DesiredResource) -> Result<&AgeSecretResource, BackendError> {
    match resource {
        DesiredResource::AgeSecret(secret) => Ok(secret),
        _ => Err(BackendError("需要 age 加密文件资源".into())),
    }
}

fn identity_collision(output: &str, identity: &str) -> BackendError {
    BackendError(format!(
        "受管路径 {output:?} 与解密 identity {identity:?} 冲突"
    ))
}

fn mode(secret: &AgeSecretResource) -> Result<u32, BackendError> {
    let mode = secret.attributes.mode.as_deref().unwrap_or("0600");
    if !(3..=4).contains(&mode.len()) || !mode.bytes().all(|b| matches!(b, b'0'..=b'7')) {
        return Err(BackendError(format!("无效的加密文件权限 {mode:?}")));
    }
    Ok(u32::from_str_radix(mode, 8).expect("validated octal mode"))
}

fn attribute_drift(
    secret: &AgeSecretResource,
    actual: &AgeSecretObservation,
) -> Result<Vec<&'static str>, BackendError> {
    let AgeSecretObservation::Present {
        uid,
        gid,
        mode: observed,
        ..
    } = actual
    else {
        return Ok(Vec::new());
    };
    let mut fields = Vec::new();
    if let Some(owner) = &secret.attributes.owner
        && accounts::user_id(owner)?
            .filter(|id| uid == &Some(*id))
            .is_none()
    {
        fields.push("属主需修正");
    }
    if let Some(group) = &secret.attributes.group
        && accounts::group_id(group)?
            .filter(|id| gid == &Some(*id))
            .is_none()
    {
        fields.push("属组需修正");
    }
    if mode(secret)? != *observed {
        fields.push("权限需修正");
    }
    Ok(fields)
}
