//! Observations available to explicit SDK target queries.

use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{self, Read},
    path::Path,
};

use dek_core::{Observation, protocol::TargetQueryKind, target_protocol::MAX_FRAME_BYTES};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::backend::{BackendError, BackendRegistry};

// Leave room for the response wrapper, observation digest and request ID.
const MAX_WIRE_TEXT_BYTES: usize = MAX_FRAME_BYTES - 1024;

pub fn observe(
    registry: &BackendRegistry<'_>,
    kind: TargetQueryKind,
    key: &str,
) -> Result<(Value, Observation), BackendError> {
    observe_input(registry, kind, key, None)
}

pub(crate) fn observe_remote(
    registry: &BackendRegistry<'_>,
    kind: TargetQueryKind,
    key: &str,
) -> Result<(Value, Observation), BackendError> {
    observe_input(registry, kind, key, Some(MAX_WIRE_TEXT_BYTES))
}

fn observe_input(
    registry: &BackendRegistry<'_>,
    kind: TargetQueryKind,
    key: &str,
    wire_limit: Option<usize>,
) -> Result<(Value, Observation), BackendError> {
    let target = registry.target();
    let value = match kind {
        TargetQueryKind::Fact => match key {
            "architecture" => json!(target.architecture),
            "os" => json!(target.os),
            "distribution" => json!(target.distribution),
            "id" => json!(target.id),
            _ => Value::Null,
        },
        TargetQueryKind::Capability => {
            let supported = registry.capabilities().resources.iter().any(|capability| {
                json!(capability.kind).as_str().is_some_and(|name| {
                    key == name
                        || key
                            .strip_prefix(name)
                            .and_then(|suffix| suffix.strip_prefix('.'))
                            .is_some_and(|attribute| capability.attributes.contains(attribute))
                })
            });
            json!(supported)
        }
        TargetQueryKind::ReadText => return read_text(key, wire_limit),
    };
    let observation = if value.is_null() {
        Observation::Missing
    } else {
        let encoded = serde_json::to_vec(&value).expect("target facts are JSON values");
        digest(&encoded)
    };
    Ok((value, observation))
}

fn read_text(key: &str, wire_limit: Option<usize>) -> Result<(Value, Observation), BackendError> {
    let path = absolute_path(key)?;
    let read_error = |error| BackendError(format!("cannot read target file {key:?}: {error}"));
    let Some((mut file, _)) = open_regular(path, true).map_err(read_error)? else {
        return Ok((Value::Null, Observation::Missing));
    };
    let text = if let Some(limit) = wire_limit {
        let too_large = || {
            BackendError(format!(
                "target file {key:?} exceeds the target protocol frame limit"
            ))
        };
        let mut bytes = Vec::new();
        file.take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(read_error)?;
        if bytes.len() > limit {
            return Err(too_large());
        }
        let text = String::from_utf8(bytes)
            .map_err(|error| BackendError(format!("cannot read target file {key:?}: {error}")))?;
        if serde_json::to_string(&text)
            .expect("text encodes as JSON")
            .len()
            > limit
        {
            return Err(too_large());
        }
        text
    } else {
        let mut text = String::new();
        file.read_to_string(&mut text).map_err(read_error)?;
        text
    };
    let observation = digest(text.as_bytes());
    Ok((Value::String(text), observation))
}

fn digest(bytes: &[u8]) -> Observation {
    Observation::Digest(format!("sha256:{:x}", Sha256::digest(bytes)))
}

pub fn absolute_path(raw: &str) -> Result<&Path, BackendError> {
    let path = Path::new(raw);
    if !path.is_absolute() || raw.contains('\0') {
        return Err(BackendError(format!(
            "expected an absolute target path: {raw:?}"
        )));
    }
    Ok(path)
}

pub fn open_regular(path: &Path, follow_links: bool) -> io::Result<Option<(File, Metadata)>> {
    let metadata = if follow_links {
        fs::metadata(path)
    } else {
        fs::symlink_metadata(path)
    };
    let metadata = match metadata {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    ensure_regular(&metadata)?;

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let flags = libc::O_NONBLOCK | if follow_links { 0 } else { libc::O_NOFOLLOW };
        options.custom_flags(flags);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    ensure_regular(&metadata)?;
    Ok(Some((file, metadata)))
}

fn ensure_regular(metadata: &Metadata) -> io::Result<()> {
    if metadata.is_file() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a regular file",
        ))
    }
}
