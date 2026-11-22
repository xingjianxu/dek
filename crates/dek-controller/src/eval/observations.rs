use std::{
    collections::{HashMap, hash_map::Entry},
    env, fs, io,
    path::Path,
};

use dek_core::{InputKind, InputObservation, Observation};
use sha2::{Digest, Sha256};

use crate::cache::CacheDependency;

use super::CollectError;

pub(super) fn digest(bytes: &[u8]) -> Observation {
    Observation::Digest(format!("sha256:{:x}", Sha256::digest(bytes)))
}

pub(super) fn is_target(kind: InputKind) -> bool {
    matches!(
        kind,
        InputKind::TargetFact | InputKind::TargetCapability | InputKind::TargetFile
    )
}

pub(crate) fn unchanged(
    root: &Path,
    dependencies: &[CacheDependency],
    observe_provider: impl FnMut(&InputObservation) -> Result<Observation, String>,
) -> Result<bool, CollectError> {
    let inputs = || {
        dependencies
            .iter()
            .filter_map(|dependency| match dependency {
                CacheDependency::Input(input) => Some(input),
                _ => None,
            })
    };
    if inputs().count() != dependencies.len() {
        return Ok(false);
    }

    // A local edit can remove a previously used provider dependency altogether.
    Ok(local_unchanged(root, inputs())?
        && inputs_match(
            inputs().filter(|input| !is_local(input.kind)),
            observe_provider,
        )?)
}

pub(crate) fn local_unchanged<'a>(
    root: &Path,
    inputs: impl Iterator<Item = &'a InputObservation>,
) -> Result<bool, CollectError> {
    inputs_match(inputs.filter(|input| is_local(input.kind)), |input| {
        observe_local(root, input).map_err(|error| error.to_string())
    })
}

fn is_local(kind: InputKind) -> bool {
    matches!(
        kind,
        InputKind::File | InputKind::Directory | InputKind::Environment
    )
}

fn inputs_match<'a>(
    inputs: impl Iterator<Item = &'a InputObservation>,
    mut observe: impl FnMut(&InputObservation) -> Result<Observation, String>,
) -> Result<bool, CollectError> {
    let mut current = HashMap::new();
    for input in inputs {
        let observed = match current.entry((input.kind, input.id.as_str())) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let observation = observe(input).map_err(|message| CollectError {
                    origin: input.origin.clone(),
                    message: format!("recheck {:?} {:?}: {message}", input.kind, input.id),
                })?;
                entry.insert(observation)
            }
        };
        if *observed != input.observation {
            return Ok(false);
        }
    }
    Ok(true)
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
