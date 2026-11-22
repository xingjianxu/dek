use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use tempfile::NamedTempFile;

use crate::eval::HttpInputProvider;

use super::{CacheDigest, CacheStore, lock_record};

/// Resolves explicit HTTP inputs from the local working copy's response cache.
pub struct CachedHttpInputs<'a> {
    store: &'a CacheStore,
}

impl<'a> CachedHttpInputs<'a> {
    pub fn new(store: &'a CacheStore) -> Self {
        Self { store }
    }

    fn response_path(&self, url: &str) -> PathBuf {
        let digest = CacheDigest::of_bytes(url.as_bytes()).to_string();
        self.store
            .units
            .with_file_name("http")
            .join(&digest[..2])
            .join(format!("{digest}.txt"))
    }
}

impl HttpInputProvider for CachedHttpInputs<'_> {
    fn read(&mut self, url: &str) -> Result<Option<String>, String> {
        read_response(&self.response_path(url))
            .map_err(|error| format!("cannot read cached HTTP input {url}: {error}"))
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RefreshStats {
    pub updated: usize,
    pub unchanged: usize,
}

/// Fetches each explicitly used URL once during a refresh.
pub struct RefreshHttpInputs<'a> {
    cached: CachedHttpInputs<'a>,
    agent: ureq::Agent,
    fetched: BTreeMap<String, String>,
    stats: RefreshStats,
}

impl<'a> RefreshHttpInputs<'a> {
    pub fn new(store: &'a CacheStore) -> Self {
        let agent = ureq::Agent::config_builder()
            .max_redirects(0)
            .timeout_global(Some(Duration::from_secs(30)))
            .build()
            .into();
        Self {
            cached: CachedHttpInputs::new(store),
            agent,
            fetched: BTreeMap::new(),
            stats: RefreshStats::default(),
        }
    }

    pub fn stats(&self) -> RefreshStats {
        self.stats
    }

    fn refresh(&mut self, url: &str) -> Result<String, String> {
        let path = self.cached.response_path(url);
        let parent = path
            .parent()
            .expect("HTTP response path has a cache directory");
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        let _lock = lock_record(&path).map_err(|error| error.to_string())?;
        let old = read_response(&path).map_err(|error| error.to_string())?;
        let mut response = self
            .agent
            .get(url)
            .call()
            .map_err(|error| error.to_string())?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }
        let body = response
            .body_mut()
            .read_to_string()
            .map_err(|error| error.to_string())?;
        if old.as_deref() == Some(&body) {
            self.stats.unchanged += 1;
        } else {
            replace_response(parent, &path, &body).map_err(|error| error.to_string())?;
            self.stats.updated += 1;
        }
        Ok(body)
    }
}

impl HttpInputProvider for RefreshHttpInputs<'_> {
    fn read(&mut self, url: &str) -> Result<Option<String>, String> {
        if let Some(body) = self.fetched.get(url) {
            return Ok(Some(body.clone()));
        }
        let body = self
            .refresh(url)
            .map_err(|message| format!("refresh HTTP input {url}: {message}"))?;
        self.fetched.insert(url.to_owned(), body.clone());
        Ok(Some(body))
    }
}

fn read_response(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(body) => Ok(Some(body)),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::InvalidData
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn replace_response(parent: &Path, path: &Path, body: &str) -> io::Result<()> {
    let mut staged = NamedTempFile::new_in(parent)?;
    staged.write_all(body.as_bytes())?;
    staged.as_file().sync_all()?;
    staged.persist(path).map_err(|error| error.error)?;
    File::open(parent)?.sync_all()
}
