use std::{
    collections::{HashMap, hash_map::Entry},
    env, fs, io,
    path::Path,
};

use dek_core::{InputKind, InputObservation, Observation};
use sha2::{Digest, Sha256};

use crate::cache::CacheDependency;

use super::{CollectError, LanguageCollector};

pub(super) fn digest(bytes: &[u8]) -> Observation {
    Observation::Digest(format!("sha256:{:x}", Sha256::digest(bytes)))
}

pub(super) fn is_target(kind: InputKind) -> bool {
    matches!(
        kind,
        InputKind::TargetFact | InputKind::TargetCapability | InputKind::TargetFile
    )
}

pub(super) fn unchanged(
    language: &mut LanguageCollector<'_>,
    root: &Path,
    dependencies: &[CacheDependency],
) -> Result<bool, CollectError> {
    let mut current = HashMap::new();
    // A local edit can remove a previously used provider dependency altogether.
    for provided in [false, true] {
        for dependency in dependencies {
            let CacheDependency::Input(input) = dependency else {
                return Ok(false);
            };
            if (is_target(input.kind) || input.kind == InputKind::Http) != provided {
                continue;
            }
            let observed = match current.entry((input.kind, input.id.as_str())) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => entry.insert(observe(language, root, input, provided)?),
            };
            if *observed != input.observation {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn observe(
    language: &mut LanguageCollector<'_>,
    root: &Path,
    input: &InputObservation,
    provided: bool,
) -> Result<Observation, CollectError> {
    let result = if provided {
        language.reobserve_provider_input(input)
    } else {
        observe_local(root, input).map_err(|error| error.to_string())
    };
    result.map_err(|message| CollectError {
        origin: input.origin.clone(),
        message: format!("recheck {:?} {:?}: {message}", input.kind, input.id),
    })
}

fn observe_local(root: &Path, input: &InputObservation) -> io::Result<Observation> {
    let path = root.join(&input.id);
    let observation = match input.kind {
        InputKind::File => fs::metadata(&path).and_then(|metadata| {
            if !metadata.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "input is not a regular file",
                ));
            }
            fs::read(&path).map(|bytes| digest(&bytes))
        }),
        InputKind::Directory => directory(&path),
        InputKind::Environment => {
            return Ok(
                env::var_os(&input.id).map_or(Observation::Missing, |value| {
                    digest(value.as_encoded_bytes())
                }),
            );
        }
        _ => unreachable!("provider inputs are handled separately"),
    };
    match observation {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Observation::Missing),
        result => result,
    }
}

fn directory(path: &Path) -> io::Result<Observation> {
    let mut names = fs::read_dir(path)?
        .map(|entry| {
            entry?.file_name().into_string().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "directory entry name is not UTF-8",
                )
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    names.sort_unstable();
    Ok(digest(
        &serde_json::to_vec(&names).expect("directory names encode as JSON strings"),
    ))
}
