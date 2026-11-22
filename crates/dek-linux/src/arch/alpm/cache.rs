use std::{
    fs, io,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

pub(super) fn matches_archive(path: &Path, sha256: &str) -> io::Result<bool> {
    let mut archive = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let mut hash = Sha256::new();
    io::copy(&mut archive, &mut hash)?;
    Ok(sha256.eq_ignore_ascii_case(&format!("{:x}", hash.finalize())))
}

pub(super) fn stage_archive(
    cachedirs: &[PathBuf],
    stage: &Path,
    filename: &str,
    sha256: &str,
) -> io::Result<bool> {
    let incoming = tempfile::NamedTempFile::new_in(stage)?;
    let signature = format!("{filename}.sig");
    for dir in cachedirs {
        let source = dir.join(filename);
        if !source.is_file() {
            continue;
        }
        fs::copy(source, incoming.path())?;
        if !matches_archive(incoming.path(), sha256)? {
            continue;
        }
        incoming.persist(stage.join(filename))?;
        let detached = dir.join(&signature);
        if detached.is_file() {
            fs::copy(detached, stage.join(signature))?;
        }
        return Ok(true);
    }
    Ok(false)
}

/// Seal files brought back by the download account before the final transaction.
pub(super) fn pin_download(stage: &Path, filename: &str) -> io::Result<()> {
    let path = stage.join(filename);
    let mut file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::other(format!(
            "预期软件包缓存为普通文件：{}",
            path.display()
        )));
    }
    if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        let mut pinned = tempfile::NamedTempFile::new_in(stage)?;
        io::copy(&mut file, &mut pinned)?;
        pinned.persist(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use sha2::{Digest, Sha256};

    #[test]
    fn uses_the_first_matching_cache_entry_even_when_an_earlier_name_collides() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let filename = "target-1-1-any.pkg.tar.zst";
        fs::write(first.path().join(filename), b"different bytes").unwrap();
        fs::write(second.path().join(filename), b"reviewed bytes").unwrap();
        fs::write(second.path().join(format!("{filename}.sig")), b"signature").unwrap();
        let directories = [first.path().to_owned(), second.path().to_owned()];
        let sha = format!("{:x}", Sha256::digest(b"reviewed bytes"));
        assert!(super::stage_archive(&directories, staging.path(), filename, &sha).unwrap());
        assert_eq!(
            fs::read(staging.path().join(filename)).unwrap(),
            b"reviewed bytes"
        );
        assert_eq!(
            fs::read(first.path().join(filename)).unwrap(),
            b"different bytes"
        );
        assert_eq!(
            fs::read(staging.path().join(format!("{filename}.sig"))).unwrap(),
            b"signature"
        );
    }
}
