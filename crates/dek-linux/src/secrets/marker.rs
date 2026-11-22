use std::{io::Read, path::Path};

use dek_core::{AgeSecretResource, AgeSecretVerification};
use dek_target::{
    backend::BackendError,
    state::{SecretVerification, StateError, StateStore},
};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use super::path::{ExistingFile, normalized_absolute};

const HEADER: &[u8] = b"dek-age-marker-v1\n";
const MARKER_SIZE: usize = HEADER.len() + 96;
type SecretMac = Hmac<Sha256>;

pub(super) fn observe(
    state_root: &Path,
    secret: &AgeSecretResource,
    output: &Path,
    file: &ExistingFile,
) -> Result<AgeSecretVerification, BackendError> {
    let id = marker_id(output);
    let binding = binding(secret, output);
    let state = |err: StateError| BackendError(format!("读取加密文件验证状态：{err}"));
    let marker = StateStore::read_secret_verification(state_root, &id).map_err(state)?;
    let current = verify_marker(marker, &binding, output, file)?;
    if current != AgeSecretVerification::Matched {
        let candidate = StateStore::read_secret_candidate(state_root, &id).map_err(state)?;
        if verify_marker(candidate, &binding, output, file)? == AgeSecretVerification::Matched {
            return Ok(AgeSecretVerification::Matched);
        }
    }
    Ok(current)
}

fn verify_marker(
    material: Option<SecretVerification>,
    binding: &[u8; 32],
    output: &Path,
    file: &ExistingFile,
) -> Result<AgeSecretVerification, BackendError> {
    let Some(material) = material else {
        return Ok(AgeSecretVerification::Unverified);
    };
    let key = Zeroizing::new(material.key);
    let marker = material.marker;
    if key.len() != 32 || marker.len() != MARKER_SIZE || !marker.starts_with(HEADER) {
        return Ok(AgeSecretVerification::Unverified);
    }

    let (saved_key_id, fields) = marker[HEADER.len()..].split_at(32);
    let key_id: [u8; 32] = Sha256::digest(&*key).into();
    if saved_key_id != key_id {
        return Ok(AgeSecretVerification::Unverified);
    }
    let (reviewed_binding, expected_mac) = fields.split_at(32);
    if reviewed_binding != binding {
        return Ok(AgeSecretVerification::InputsChanged);
    }

    let mut reader = match file.open() {
        Ok(reader) => reader,
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            return Ok(AgeSecretVerification::Unverified);
        }
        Err(_) => {
            return Err(BackendError(format!(
                "无法读取加密文件 {:?} 的验证输入",
                output
            )));
        }
    };
    let mac = content_mac(&key, binding, &mut reader)?;
    if !file
        .unchanged(&reader)
        .map_err(|_| BackendError(format!("无法复核加密文件 {:?} 的观测状态", output)))?
    {
        return Ok(AgeSecretVerification::Unverified);
    }
    if mac.verify_slice(expected_mac).is_ok() {
        Ok(AgeSecretVerification::Matched)
    } else {
        Ok(AgeSecretVerification::Drifted)
    }
}

/// Create target-local marker bytes to publish after a successful deployment.
/// The returned name is independent of plaintext and suitable for the protected state directory.
pub fn verification_marker(
    secret: &AgeSecretResource,
    key: &[u8; 32],
    plaintext: impl Read,
) -> Result<(String, Vec<u8>), BackendError> {
    let path = normalized_absolute(&secret.path)?;
    let binding = binding(secret, &path);
    let mac = content_mac(key, &binding, plaintext)?
        .finalize()
        .into_bytes();
    let mut marker = Vec::with_capacity(MARKER_SIZE);
    marker.extend_from_slice(HEADER);
    marker.extend_from_slice(&Sha256::digest(key));
    marker.extend_from_slice(&binding);
    marker.extend_from_slice(&mac);
    Ok((marker_id(&path), marker))
}

fn marker_id(path: &Path) -> String {
    format!("{:x}", Sha256::digest(path.as_os_str().as_encoded_bytes()))
}

fn binding(secret: &AgeSecretResource, normalized_path: &Path) -> [u8; 32] {
    let mut digest = Sha256::new();
    let mut add = |value: &[u8]| {
        digest.update((value.len() as u64).to_be_bytes());
        digest.update(value);
    };
    add(normalized_path.as_os_str().as_encoded_bytes());
    add(secret.ciphertext.0.as_bytes());
    for path in &secret.identity_paths {
        add(path.as_bytes());
    }
    digest.finalize().into()
}

fn content_mac(
    key: &[u8],
    binding: &[u8; 32],
    mut reader: impl Read,
) -> Result<SecretMac, BackendError> {
    let mut mac = SecretMac::new_from_slice(key).expect("HMAC supports all key sizes");
    mac.update(HEADER);
    mac.update(binding);
    let mut buffer = Zeroizing::new([0; 8192]);
    loop {
        let read = reader
            .read(&mut *buffer)
            .map_err(|_| BackendError("无法读取加密文件的验证输入".into()))?;
        if read == 0 {
            return Ok(mac);
        }
        mac.update(&buffer[..read]);
    }
}
