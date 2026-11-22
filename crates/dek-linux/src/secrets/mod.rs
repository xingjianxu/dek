//! Plan age resources from ciphertext metadata and target-local verification markers.

mod accounts;
mod marker;
mod path;

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use dek_core::{
    ActionKind, AgeSecretObservation, AgeSecretResource, AgeSecretVerification, DesiredResource,
    Precondition, ResourceCapability, ResourceId, ResourceKind,
};
use dek_target::backend::{Backend, BackendError, PlannedAction};

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
        let output = CheckedPath::inspect(&secret.path)?;
        let Some(file) = output.file else {
            return Ok(AgeSecretObservation::Missing);
        };
        let verification = marker::observe(&self.state_root, secret, &output.normalized, &file)?;
        Ok(AgeSecretObservation::Present {
            verification,
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
        for identity_path in &secret.identity_paths {
            let identity = CheckedPath::identity(identity_path)?;
            for resource in managed {
                let ResourceId::Path(path) = resource else {
                    continue;
                };
                if Path::new(path) == identity.normalized {
                    return Err(identity_collision(path, identity_path));
                }
                let Some(file) = &identity.file else { continue };
                match fs::metadata(path) {
                    Ok(metadata) if file.matches_metadata(&metadata) => {
                        return Err(identity_collision(path, identity_path));
                    }
                    Ok(_) => {}
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                    Err(err) => return Err(BackendError(format!("检查受管路径 {path:?}：{err}"))),
                }
            }
        }
        Ok(())
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

    fn execute(&self, _: &ActionKind) -> Result<(), BackendError> {
        Err(BackendError("此后端只支持加密文件的规划与核验".into()))
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
