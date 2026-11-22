use std::{
    env, fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

use dek_core::protocol::EVALUATOR_PROTOCOL_VERSION;
use serde_json::json;

use crate::cache::{CacheDigest, semantic_digest};

pub(super) fn signature(program: &Path, sdk: &str) -> Option<String> {
    let executable = fs::canonicalize(resolve(program)?).ok()?;
    let metadata = fs::metadata(&executable).ok()?;
    if !is_executable(&metadata) {
        return None;
    }
    let bytes = fs::read(&executable).ok()?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|time| (time.as_secs(), time.subsec_nanos()));
    let ruby_environment = [
        "RUBYOPT",
        "RUBYLIB",
        "GEM_HOME",
        "GEM_PATH",
        "RUBYGEMS_GEMDEPS",
    ]
    .map(|name| env::var_os(name).map(|value| fingerprint(value.as_encoded_bytes())));
    let runtime = json!([
        EVALUATOR_PROTOCOL_VERSION,
        fingerprint(sdk.as_bytes()),
        fingerprint(executable.as_os_str().as_encoded_bytes()),
        fingerprint(&bytes),
        modified,
        ruby_environment,
    ]);
    Some(format!("ruby-collect-v1:{}", semantic_digest(&runtime)))
}

fn fingerprint(bytes: &[u8]) -> String {
    CacheDigest::of_bytes(bytes).to_string()
}

fn resolve(program: &Path) -> Option<PathBuf> {
    if program.is_absolute() || program.components().count() > 1 {
        return Some(program.to_path_buf());
    }

    let path = env::var_os("PATH").unwrap_or_else(|| "/bin:/usr/bin".into());
    env::split_paths(&path)
        .map(|directory| directory.join(program))
        .find(|candidate| fs::metadata(candidate).is_ok_and(|metadata| is_executable(&metadata)))
}

fn is_executable(metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}
