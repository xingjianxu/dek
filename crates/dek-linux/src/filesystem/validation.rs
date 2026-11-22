use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{Seek, Write},
    path::PathBuf,
};

use dek_core::{ActionKind, DesiredResource, FileAttributes, FileState, ResourceSet};
use dek_target::backend::{ActionContext, BackendError};
use rustix::fs::{MemfdFlags, SealFlags, fcntl_add_seals, memfd_create};

use super::normalize;

pub struct ValidationInput {
    pub path: PathBuf,
    pub attributes: FileAttributes,
    /// Staged bytes are readable and sealed against further writes.
    pub file: File,
}

pub struct FileValidationGroup {
    paths: BTreeSet<PathBuf>,
    check: fn(&[ValidationInput]) -> Result<(), BackendError>,
}

impl FileValidationGroup {
    /// Register a trusted target-side validator for the managed paths.
    pub fn new(
        paths: impl IntoIterator<Item = String>,
        check: fn(&[ValidationInput]) -> Result<(), BackendError>,
    ) -> Result<Self, BackendError> {
        let paths = paths
            .into_iter()
            .map(|path| normalize(&path))
            .collect::<Result<_, _>>()?;
        Ok(Self { paths, check })
    }
}

pub(super) fn check_groups(
    groups: &[FileValidationGroup],
    desired: &ResourceSet,
    actions: &[ActionContext<'_>],
) -> Result<(), BackendError> {
    let Some(context) = actions.first() else {
        return Ok(());
    };
    if groups.is_empty() {
        return Ok(());
    }
    let changed = actions
        .iter()
        .filter_map(|context| match &context.action.kind {
            ActionKind::WriteFile { path, .. } => Some(normalize(path)),
            _ => None,
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    if changed.is_empty() {
        return Ok(());
    }

    let mut reviewed = BTreeMap::new();
    for resource in &desired.resources {
        if let DesiredResource::File(resource) = &resource.desired
            && let FileState::Present(content) = &resource.state
        {
            reviewed.insert(normalize(&resource.path)?, content);
        }
    }

    for group in groups {
        if group.paths.is_disjoint(&changed) {
            continue;
        }
        let mut inputs = Vec::new();
        for path in &group.paths {
            let content = reviewed
                .get(path)
                .ok_or_else(|| BackendError(format!("组内文件 {path:?} 缺少已审阅内容")))?;
            let bytes = context.content(&content.digest)?;
            let file = stage(bytes)
                .map_err(|err| BackendError(format!("无法暂存组内文件 {path:?}：{err}")))?;
            inputs.push(ValidationInput {
                path: path.clone(),
                attributes: content.attributes.clone(),
                file,
            });
        }
        (group.check)(&inputs)?;
    }
    Ok(())
}

fn stage(bytes: &[u8]) -> std::io::Result<File> {
    let fd = memfd_create(
        "dek-file-validation",
        MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
    )?;
    let mut file = File::from(fd);
    file.write_all(bytes)?;
    fcntl_add_seals(
        &file,
        SealFlags::WRITE | SealFlags::GROW | SealFlags::SHRINK | SealFlags::SEAL,
    )?;
    file.rewind()?;
    Ok(file)
}
