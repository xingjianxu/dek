use std::{
    fs,
    io::{self, Write},
    path::{Component, Path},
};

use serde_json::json;
use tempfile::NamedTempFile;

use super::{MANIFEST_FILE, RepoError, manifest::SCHEMA_VERSION};

#[derive(Debug, Clone, Copy)]
pub enum EntryLanguage {
    Json,
    Ruby,
}

/// 创建本机配置仓库，保留任何现有的同名文件。
pub fn initialize(root: &Path, hostname: &str, language: EntryLanguage) -> Result<(), RepoError> {
    if hostname.trim().is_empty()
        || !matches!(
            Path::new(hostname).components().next(),
            Some(Component::Normal(name)) if name == hostname
        )
        || hostname.chars().any(|c| c.is_control() || c == '\\')
    {
        return Err(RepoError::InvalidHostname(hostname.to_owned()));
    }

    let (extension, entry_contents) = match language {
        EntryLanguage::Json => (
            "json",
            format!(
                "{:#}\n",
                json!({ "schemaVersion": SCHEMA_VERSION, "imports": [], "options": [] })
            ),
        ),
        EntryLanguage::Ruby => (
            "rb",
            "# 在此添加本机配置。\n# configure \"programs.git\", enable: true\n".to_owned(),
        ),
    };
    let entry = format!("hosts/{hostname}/default.{extension}");
    let entry_path = root.join(&entry);
    let manifest_path = root.join(MANIFEST_FILE);
    ensure_absent(&manifest_path)?;
    let hosts = root.join("hosts");
    check_directory(&hosts)?;
    check_directory(&hosts.join(hostname))?;
    ensure_absent(&entry_path)?;

    let mut host = json!({ "entry": entry, "target": { "local": true } });
    if matches!(language, EntryLanguage::Ruby) {
        host["inputs"] = json!("sdk");
    }
    let manifest = json!({ "schemaVersion": SCHEMA_VERSION, "hosts": { hostname: host } });
    let manifest_contents = format!("{manifest:#}\n");
    let staged_entry = stage(&entry_path, &entry_contents)?;
    let staged_manifest = stage(&manifest_path, &manifest_contents)?;

    publish(staged_entry, &entry_path)?;
    if let Err(original) = publish(staged_manifest, &manifest_path) {
        // 清单最后发布，避免暴露一个指向未建好入口的仓库。
        if let Err(source) = fs::remove_file(&entry_path)
            && source.kind() != io::ErrorKind::NotFound
        {
            return Err(RepoError::InitRollback {
                entry: entry_path,
                original: Box::new(original),
                source,
            });
        }
        return Err(original);
    }
    Ok(())
}

fn ensure_absent(path: &Path) -> Result<(), RepoError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(RepoError::InitConflict(path.to_owned())),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(RepoError::io("无法检查初始化路径", path, err)),
    }
}

fn check_directory(path: &Path) -> Result<(), RepoError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(RepoError::InitConflict(path.to_owned())),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(RepoError::io("无法检查初始化目录", path, err)),
    }
}

fn stage(path: &Path, contents: &str) -> Result<NamedTempFile, RepoError> {
    let parent = path.parent().expect("初始化路径含父目录");
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    fs::create_dir_all(parent).map_err(|err| RepoError::io("无法创建初始化目录", parent, err))?;
    let mut file = NamedTempFile::new_in(parent)
        .map_err(|err| RepoError::io("无法创建待写入的配置文件", path, err))?;
    file.write_all(contents.as_bytes())
        .map_err(|err| RepoError::io("无法写入配置文件", path, err))?;
    Ok(file)
}

fn publish(file: NamedTempFile, path: &Path) -> Result<(), RepoError> {
    file.persist_noclobber(path).map(drop).map_err(|err| {
        if err.error.kind() == io::ErrorKind::AlreadyExists {
            RepoError::InitConflict(path.to_owned())
        } else {
            RepoError::io("无法保存配置文件", path, err.error)
        }
    })
}
