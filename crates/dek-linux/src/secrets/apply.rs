use std::{io::Read, path::Path};

use dek_core::{ActionKind, AgeSecretResource};
use dek_target::{
    backend::{ActionContext, BackendError},
    state::StateStore,
};
use rustix::{
    fs::{Mode, Stat, fchmod, fchown, fstat},
    process::{Gid, Uid, geteuid},
};
use zeroize::Zeroizing;

use crate::{
    accounts::{group_id, user_id},
    filesystem::atomic_file::{StagedFile, TargetPath, same_revision},
};

use super::{mode, path::CheckedPath, verification_marker};

pub(super) fn apply(
    state_root: &Path,
    secret: &AgeSecretResource,
    output: &CheckedPath,
    context: &ActionContext<'_>,
) -> Result<bool, BackendError> {
    let target = TargetPath::open(&output.normalized)
        .map_err(|err| access_error(secret, err))?
        .ok_or_else(|| BackendError(format!("加密文件的目标父目录尚未创建 {:?}", secret.path)))?;
    let expected = output.file.as_ref().map(|file| &file.stat);
    let attributes = Attributes::resolve(secret, expected)?;
    if matches!(context.action.kind, ActionKind::SetAgeSecretAttributes(_)) {
        let expected = expected.ok_or_else(|| BackendError("待修正的加密文件已不存在".into()))?;
        attributes
            .update(&target, expected)
            .map_err(|err| access_error(secret, err))?;
        return Ok(true);
    }

    let identities = safe_identities(secret, output)?;
    let ciphertext = context.content(&secret.ciphertext)?;
    let plaintext = decrypt(ciphertext, &identities)?;
    let contents_match = match expected {
        Some(prior) => {
            matches_contents(&target, prior, &plaintext).map_err(|err| access_error(secret, err))?
        }
        None => false,
    };

    // Prepare and validate state storage before changing the destination.
    let store = StateStore::open(state_root).map_err(state_error)?;
    let (id, marker) = {
        let state = store.lock().map_err(state_error)?;
        let key = Zeroizing::new(state.secret_verification_key().map_err(state_error)?);
        verification_marker(secret, &key, plaintext.as_slice())?
    };

    let mut changed = false;
    if contents_match {
        let prior = expected.expect("identical content requires an existing file");
        if !attributes.matches(prior) {
            attributes
                .update(&target, prior)
                .map_err(|err| access_error(secret, err))?;
            changed = true;
        }
    } else {
        let stage = StagedFile::new(
            &target,
            &plaintext,
            attributes.uid,
            attributes.gid,
            attributes.mode,
        )
        .map_err(|err| access_error(secret, err))?;
        // Recheck identities after decryption and staging, immediately before publication.
        let current = CheckedPath::inspect(&secret.path)?;
        let _ = safe_identities(secret, &current)?;
        let current_parent =
            TargetPath::open(&current.normalized).map_err(|err| access_error(secret, err))?;
        if !current_parent
            .as_ref()
            .map_or(Ok(false), |fresh| target.same_parent(fresh))
            .map_err(|err| access_error(secret, err))?
        {
            return Err(BackendError(format!(
                "加密文件的目标父目录已变化 {:?}",
                secret.path
            )));
        }
        target
            .require_unchanged(expected)
            .map_err(|err| access_error(secret, err))?;
        store
            .lock()
            .map_err(state_error)?
            .stage_secret_verification(&id, &marker)
            .map_err(state_error)?;
        context.before_content_change()?;
        stage
            .publish(expected)
            .map_err(|err| access_error(secret, err))?;
        changed = true;
    }

    let observed = target
        .observe()
        .map_err(|err| access_error(secret, err))?
        .ok_or_else(|| BackendError("部署后找不到加密文件".into()))?;
    if !attributes.matches(&observed)
        || !matches_contents(&target, &observed, &plaintext)
            .map_err(|err| access_error(secret, err))?
    {
        return Err(BackendError("加密文件部署后未通过核验".into()));
    }
    store
        .lock()
        .map_err(state_error)?
        .write_secret_verification(&id, &marker)
        .map_err(state_error)?;
    Ok(changed)
}

fn safe_identities(
    secret: &AgeSecretResource,
    output: &CheckedPath,
) -> Result<Vec<CheckedPath>, BackendError> {
    secret
        .identity_paths
        .iter()
        .map(|path| {
            let identity = CheckedPath::identity(path)?;
            if output.aliases(&identity) {
                return Err(super::identity_collision(&secret.path, path));
            }
            let file = identity
                .file
                .as_ref()
                .ok_or_else(|| BackendError(format!("目标机缺少可读取的 age identity {path:?}")))?;
            if file.stat.st_uid != geteuid().as_raw() || file.stat.st_mode & 0o077 != 0 {
                return Err(BackendError(format!(
                    "age identity 的属主或访问权限不安全 {path:?}"
                )));
            }
            Ok(identity)
        })
        .collect()
}

fn decrypt(ciphertext: &[u8], paths: &[CheckedPath]) -> Result<Zeroizing<Vec<u8>>, BackendError> {
    let mut identities = Vec::new();
    for path in paths {
        let path_error = || {
            BackendError(format!(
                "无法读取有效的 X25519 identity {:?}",
                path.normalized
            ))
        };
        let file = path.file.as_ref().expect("validated identity file");
        let mut reader = file.open().map_err(|_| path_error())?;
        // Identity files contain short textual keys and optional comments.
        const MAX_IDENTITY_BYTES: u64 = 1024 * 1024;
        let mut contents = Zeroizing::new(Vec::new());
        (&mut reader)
            .take(MAX_IDENTITY_BYTES + 1)
            .read_to_end(&mut contents)
            .map_err(|_| path_error())?;
        if contents.len() as u64 > MAX_IDENTITY_BYTES
            || !file.unchanged(&reader).map_err(|_| path_error())?
        {
            return Err(path_error());
        }
        let first = identities.len();
        for line in std::str::from_utf8(&contents)
            .map_err(|_| path_error())?
            .lines()
        {
            let line = line.trim();
            if !line.is_empty() && !line.starts_with('#') {
                identities.push(
                    line.parse::<age::x25519::Identity>()
                        .map_err(|_| path_error())?,
                );
            }
        }
        if identities.len() == first {
            return Err(path_error());
        }
    }
    let decryptor = age::Decryptor::new(age::armor::ArmoredReader::new(ciphertext))
        .map_err(|_| BackendError("无法读取审阅过的 age 密文".into()))?;
    let mut reader = decryptor
        .decrypt(identities.iter().map(|key| key as &dyn age::Identity))
        .map_err(|_| BackendError("配置的 age identity 无法解密此密文".into()))?;
    let mut plaintext = Zeroizing::new(Vec::with_capacity(ciphertext.len()));
    reader
        .read_to_end(&mut plaintext)
        .map_err(|_| BackendError("age 密文解密或完整性校验失败".into()))?;
    Ok(plaintext)
}

fn matches_contents(target: &TargetPath, prior: &Stat, bytes: &[u8]) -> std::io::Result<bool> {
    if prior.st_size as u64 != bytes.len() as u64 {
        return Ok(false);
    }
    let mut file = target.read(prior)?;
    let mut buffer = Zeroizing::new([0; 8192]);
    let mut same = true;
    for chunk in bytes.chunks(buffer.len()) {
        file.read_exact(&mut buffer[..chunk.len()])?;
        same &= *chunk == buffer[..chunk.len()];
    }
    if !same_revision(prior, &fstat(&file)?) {
        return Err(std::io::Error::other("读取期间加密文件发生变化"));
    }
    Ok(same)
}

struct Attributes {
    uid: Option<Uid>,
    gid: Option<Gid>,
    mode: Mode,
}

impl Attributes {
    fn resolve(secret: &AgeSecretResource, prior: Option<&Stat>) -> Result<Self, BackendError> {
        let uid = match &secret.attributes.owner {
            Some(name) => {
                Some(user_id(name)?.ok_or_else(|| BackendError(format!("找不到属主 {name:?}")))?)
            }
            None => prior.map(|stat| stat.st_uid),
        };
        let gid = match &secret.attributes.group {
            Some(name) => {
                Some(group_id(name)?.ok_or_else(|| BackendError(format!("找不到属组 {name:?}")))?)
            }
            None => prior.map(|stat| stat.st_gid),
        };
        Ok(Self {
            uid: uid.map(Uid::from_raw),
            gid: gid.map(Gid::from_raw),
            mode: Mode::from_bits_truncate(mode(secret)?),
        })
    }

    fn matches(&self, stat: &Stat) -> bool {
        self.uid.is_none_or(|uid| uid.as_raw() == stat.st_uid)
            && self.gid.is_none_or(|gid| gid.as_raw() == stat.st_gid)
            && self.mode.bits() == stat.st_mode & 0o7777
    }

    fn update(&self, target: &TargetPath, prior: &Stat) -> std::io::Result<()> {
        let file = target.read(prior)?;
        let uid = self.uid.filter(|uid| uid.as_raw() != prior.st_uid);
        let gid = self.gid.filter(|gid| gid.as_raw() != prior.st_gid);
        if uid.is_some() || gid.is_some() {
            fchown(&file, uid, gid)?;
        }
        fchmod(&file, self.mode)?;
        file.sync_all()
    }
}

fn access_error(secret: &AgeSecretResource, err: std::io::Error) -> BackendError {
    BackendError(format!("无法操作加密文件 {:?}：{err}", secret.path))
}

fn state_error(err: dek_target::state::StateError) -> BackendError {
    BackendError(format!("无法保存加密文件验证状态：{err}"))
}
