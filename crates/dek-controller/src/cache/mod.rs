//! 管理机按工作副本和求值单元保存的可重建缓存。

#[cfg(feature = "benchmark-cache")]
mod benchmark;
mod digest;
mod http;

use std::{
    collections::BTreeMap,
    env,
    error::Error,
    fs::{self, File},
    io::{self, Cursor, Write},
    path::{self, Path, PathBuf},
};

use dek_core::{InputObservation, OptionPath};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use tempfile::NamedTempFile;
use thiserror::Error;

#[cfg(feature = "benchmark-cache")]
pub use benchmark::compare_cache_records;
pub use digest::{CacheDigest, canonical_set, semantic_digest};
pub use http::{CachedHttpInputs, RefreshHttpInputs, RefreshStats};

const FORMAT_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum UnitStage {
    Collect,
    Merge,
    Expand,
    TargetSnapshot,
}

/// Stable identity of an evaluation unit across evaluations.
///
/// The caller supplies a normalized identifier and a target identity when
/// the unit depends on target-specific inputs.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct UnitKey {
    pub stage: UnitStage,
    pub unit: String,
    pub params: BTreeMap<String, Value>,
    pub target: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CacheDependency {
    Input(InputObservation),
    Unit {
        key: UnitKey,
        semantic_digest: CacheDigest,
    },
    /// The effective options visible to a module. None means all visible options.
    Options {
        paths: Option<Vec<OptionPath>>,
        semantic_digest: CacheDigest,
    },
}

/// Typed evaluation output, its semantic digest, and the inputs used to compute it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CacheEntry<T> {
    pub dependencies: Vec<CacheDependency>,
    pub semantic_digest: CacheDigest,
    pub output: T,
}

/// One read of a unit, with the revision required for a later publication.
#[derive(Debug)]
pub struct CacheLookup<T> {
    /// A compatible entry, whose observed inputs the caller must still recheck.
    pub entry: Option<CacheEntry<T>>,
    path: PathBuf,
    key: UnitKey,
    evaluator_signature: String,
    revision: Option<CacheDigest>,
}

impl<T> CacheLookup<T> {
    pub(crate) fn revision(&self) -> Option<CacheDigest> {
        self.revision
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Publication {
    Published,
    Superseded,
    InputsChanged,
}

pub type RecheckError = Box<dyn Error + Send + Sync>;

#[derive(Debug, Error)]
pub enum CacheError {
    #[error("无法定位求值缓存目录，请设置绝对路径 XDG_CACHE_HOME 或 HOME")]
    CacheHomeUnavailable,
    #[error("{action} {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("无法编码求值缓存 {path}: {source}")]
    Encode {
        path: PathBuf,
        #[source]
        source: rmp_serde::encode::Error,
    },
    #[cfg(feature = "benchmark-cache")]
    #[error("无法编码基准 JSON 缓存 {path}: {source}")]
    EncodeBenchmark {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("无法复核求值缓存的依赖 {path}: {source}")]
    Recheck {
        path: PathBuf,
        #[source]
        source: RecheckError,
    },
    #[error("查询记录属于其他求值缓存：{0}")]
    ForeignLookup(PathBuf),
}

impl CacheError {
    fn io(action: &'static str, path: &Path, source: io::Error) -> Self {
        Self::Io {
            action,
            path: path.to_owned(),
            source,
        }
    }
}

#[derive(Clone, Debug)]
pub struct CacheStore {
    repository: PathBuf,
    units: PathBuf,
    #[cfg(feature = "benchmark-cache")]
    benchmark_json: bool,
}

impl CacheStore {
    /// Locate the Linux cache for an existing configuration worktree.
    pub fn for_repository(repo_root: &Path) -> Result<Self, CacheError> {
        let cache_home = env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| {
                env::var_os("HOME")
                    .map(PathBuf::from)
                    .filter(|path| path.is_absolute())
                    .map(|home| home.join(".cache"))
            })
            .ok_or(CacheError::CacheHomeUnavailable)?;
        Self::new(&cache_home, repo_root)
    }

    /// Use an explicit cache home with the same repository and unit layout as the default.
    pub fn new(cache_home: &Path, repo_root: &Path) -> Result<Self, CacheError> {
        let cache_home = path::absolute(cache_home)
            .map_err(|err| CacheError::io("无法定位缓存目录", cache_home, err))?;
        let repo = fs::canonicalize(repo_root)
            .map_err(|err| CacheError::io("无法定位仓库工作副本", repo_root, err))?;
        let repo_id = CacheDigest::of_bytes(repo.as_os_str().as_encoded_bytes()).to_string();
        Ok(Self {
            repository: repo,
            units: cache_home.join("dek").join(repo_id).join("units"),
            #[cfg(feature = "benchmark-cache")]
            benchmark_json: env::var_os("DEK_BENCH_CACHE_ENCODING")
                .is_some_and(|value| value == "json"),
        })
    }

    pub fn repository_root(&self) -> &Path {
        &self.repository
    }

    pub(crate) fn revision(&self, key: &UnitKey) -> Result<Option<CacheDigest>, CacheError> {
        Ok(read_record(&self.record_path(key))?
            .as_deref()
            .map(CacheDigest::of_bytes))
    }

    pub fn record_path(&self, key: &UnitKey) -> PathBuf {
        let key = serde_json::to_value(key).expect("unit keys contain JSON values");
        let digest = semantic_digest(&key).to_string();
        let extension = "mpk";
        #[cfg(feature = "benchmark-cache")]
        let extension = if self.benchmark_json {
            "json"
        } else {
            extension
        };
        self.units
            .join(&digest[..2])
            .join(format!("{digest}.{extension}"))
    }

    /// Read a single record and capture its revision before evaluation.
    pub fn lookup<T: DeserializeOwned>(
        &self,
        key: &UnitKey,
        evaluator_signature: &str,
    ) -> Result<CacheLookup<T>, CacheError> {
        let path = self.record_path(key);
        let data = read_record(&path)?;
        let revision = data.as_deref().map(CacheDigest::of_bytes);
        let entry = data
            .as_deref()
            .and_then(|data| {
                #[cfg(feature = "benchmark-cache")]
                if self.benchmark_json {
                    return serde_json::from_slice(data).ok();
                }
                decode_messagepack::<OwnedRecord<T>>(data)
            })
            .filter(|record| {
                record.format_version == FORMAT_VERSION
                    && record.key == *key
                    && record.evaluator_signature == evaluator_signature
            })
            .map(|record| record.entry);
        Ok(CacheLookup {
            entry,
            path,
            key: key.clone(),
            evaluator_signature: evaluator_signature.to_owned(),
            revision,
        })
    }

    /// Stage a complete record and publish it if the prior revision and inputs still match.
    ///
    /// The callback receives the candidate's complete dependency list while the
    /// unit's write lock is held. This read-only callback can query dependencies,
    /// including other cached units, to confirm the current observations match.
    pub fn publish<T: Serialize>(
        &self,
        lookup: &CacheLookup<T>,
        entry: &CacheEntry<T>,
        recheck: impl FnOnce(&[CacheDependency]) -> Result<bool, RecheckError>,
    ) -> Result<Publication, CacheError> {
        self.publish_with_revision(lookup, entry, recheck)
            .map(|(result, _)| result)
    }

    pub(crate) fn publish_with_revision<T: Serialize>(
        &self,
        lookup: &CacheLookup<T>,
        entry: &CacheEntry<T>,
        recheck: impl FnOnce(&[CacheDependency]) -> Result<bool, RecheckError>,
    ) -> Result<(Publication, Option<CacheDigest>), CacheError> {
        let path = &lookup.path;
        if self.record_path(&lookup.key) != *path {
            return Err(CacheError::ForeignLookup(path.clone()));
        }

        let record = WireRecord {
            format_version: FORMAT_VERSION,
            key: &lookup.key,
            evaluator_signature: &lookup.evaluator_signature,
            entry,
        };
        let data = self.encode_for_publication(path, &record)?;

        let parent = path.parent().expect("unit record path has a parent");
        fs::create_dir_all(parent)
            .map_err(|err| CacheError::io("无法创建求值缓存目录", parent, err))?;
        let mut staged = NamedTempFile::new_in(parent)
            .map_err(|err| CacheError::io("无法创建缓存临时文件", path, err))?;
        staged
            .write_all(&data)
            .and_then(|()| staged.as_file().sync_all())
            .map_err(|err| CacheError::io("无法写入缓存临时文件", path, err))?;

        let _lock = lock_record(path)?;
        if read_record(path)?.as_deref().map(CacheDigest::of_bytes) != lookup.revision {
            return Ok((Publication::Superseded, None));
        }
        if !recheck(&entry.dependencies).map_err(|source| CacheError::Recheck {
            path: path.clone(),
            source,
        })? {
            return Ok((Publication::InputsChanged, None));
        }

        staged
            .persist(path)
            .map_err(|err| CacheError::io("无法替换求值缓存", path, err.error))?;
        File::open(parent)
            .and_then(|dir| dir.sync_all())
            .map_err(|err| CacheError::io("缓存已替换但无法同步目录", parent, err))?;
        Ok((Publication::Published, Some(CacheDigest::of_bytes(&data))))
    }

    fn encode_for_publication(
        &self,
        path: &Path,
        record: &impl Serialize,
    ) -> Result<Vec<u8>, CacheError> {
        #[cfg(feature = "benchmark-cache")]
        if self.benchmark_json {
            return serde_json::to_vec(record).map_err(|source| CacheError::EncodeBenchmark {
                path: path.to_owned(),
                source,
            });
        }
        rmp_serde::to_vec(record).map_err(|source| CacheError::Encode {
            path: path.to_owned(),
            source,
        })
    }
}

#[derive(Deserialize, Serialize)]
struct WireRecord<K, S, E> {
    format_version: u32,
    key: K,
    evaluator_signature: S,
    entry: E,
}

type OwnedRecord<T> = WireRecord<UnitKey, String, CacheEntry<T>>;

fn decode_messagepack<T: DeserializeOwned>(data: &[u8]) -> Option<T> {
    let mut reader = Cursor::new(data);
    let record = rmp_serde::from_read(&mut reader).ok()?;
    (reader.position() == data.len() as u64).then_some(record)
}

fn read_record(path: &Path) -> Result<Option<Vec<u8>>, CacheError> {
    match fs::read(path) {
        Ok(data) => Ok(Some(data)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(CacheError::io("无法读取求值缓存", path, err)),
    }
}

fn lock_record(record_path: &Path) -> Result<File, CacheError> {
    let path = record_path.with_extension("lock");
    let lock = File::options()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|err| CacheError::io("无法打开求值缓存锁", &path, err))?;
    lock.lock()
        .map_err(|err| CacheError::io("无法获取求值缓存锁", &path, err))?;
    Ok(lock)
}
