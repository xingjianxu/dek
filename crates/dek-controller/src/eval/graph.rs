use std::{
    collections::{BTreeMap, HashMap},
    fmt::{self, Write},
    fs, io,
    path::{Path, PathBuf},
};

use dek_core::{Origin, Priority, UnitSpec};
use serde_json::Value;
use thiserror::Error;

use crate::cache::CacheDigest;
use crate::repo::{HostSelector, InputPolicy, RepoError, Repository, Target};

/// 一次声明采集的上下文。入口 ID 使用仓库内的规范相对路径。
pub struct CollectRequest<'a> {
    pub repo_root: &'a Path,
    pub entry: &'a str,
    pub name: &'a str,
    pub priority: Priority,
    pub params: &'a BTreeMap<String, Value>,
    pub input_policy: Option<InputPolicy>,
    pub host: &'a str,
    pub target: &'a Target,
    pub target_id: Option<&'a str>,
}

impl CollectRequest<'_> {
    pub fn entry_path(&self) -> PathBuf {
        self.repo_root.join(self.entry)
    }
}

/// 语言适配器采集单个单元，并报告该次采集观察到的输入。
pub trait Collector {
    fn collect(&mut self, request: &CollectRequest<'_>) -> Result<UnitSpec, CollectError>;

    /// Revision of the record returned by the most recent successful collection.
    fn last_cache_revision(&self) -> Option<CacheDigest> {
        None
    }
}

#[derive(Debug)]
pub struct CollectError {
    pub origin: Option<Origin>,
    pub message: String,
}

impl fmt::Display for CollectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(origin) = &self.origin {
            write!(f, "{origin} ")?;
        }
        f.write_str(&self.message)
    }
}

impl std::error::Error for CollectError {}

#[derive(Debug, Error)]
pub enum GraphError {
    #[error(transparent)]
    Host(#[from] RepoError),
    #[error("无法定位入口 {path}: {source}")]
    EntryIo {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("入口 {0} 位于仓库外")]
    OutsideRepository(PathBuf),
    #[error("入口 {0} 的路径需要有效的 UTF-8")]
    EntryEncoding(PathBuf),
    #[error("采集入口 {entry} 失败。{source}")]
    Collect {
        entry: String,
        #[source]
        source: CollectError,
    },
    #[error("未知 profile {name:?}。导入链 {trace}")]
    UnknownProfile { name: String, trace: String },
    #[error("profile 循环引用。导入链 {trace}")]
    ProfileCycle { trace: String },
    #[error("{source}。导入链 {trace}")]
    ImportedUnit {
        trace: String,
        #[source]
        source: Box<GraphError>,
    },
}

#[derive(Debug)]
pub struct CollectedUnit {
    pub name: String,
    pub entry: String,
    pub priority: Priority,
    pub params: BTreeMap<String, Value>,
    pub input_policy: Option<InputPolicy>,
    pub spec: UnitSpec,
    pub cache_revision: Option<CacheDigest>,
    pub(crate) target_id: Option<String>,
    /// 每条 `spec.imports` 对应的 `DeclarationGraph::profiles` 下标。
    pub dependencies: Vec<usize>,
}

#[derive(Debug)]
pub struct DeclarationGraph {
    pub host: CollectedUnit,
    pub target: Target,
    /// 按首次遍历顺序，先列被依赖的 profile，再列引用它的 profile。
    pub profiles: Vec<CollectedUnit>,
}

/// 展开选定主机的声明图。每个 profile 别名与规范化参数的组合采集一次。
pub fn collect_graph(
    repo: &Repository,
    selector: HostSelector<'_>,
    collector: &mut dyn Collector,
) -> Result<DeclarationGraph, GraphError> {
    let (name, host) = repo.select_host(selector)?;
    let mut builder = GraphBuilder {
        repo,
        host: name,
        target: &host.target,
        target_id: match host.target {
            Target::Local => None,
            Target::Ssh(_) => host.target_id.as_deref(),
        },
        collector,
    };
    let host = builder.collect_unit(
        name,
        &host.entry,
        Priority::Host,
        BTreeMap::new(),
        host.inputs,
    )?;
    builder.build(host)
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct ProfileKey {
    name: String,
    params: String,
}

enum Visit {
    Active,
    Complete(usize),
}

struct Pending {
    key: Option<ProfileKey>,
    unit: CollectedUnit,
}

struct GraphBuilder<'a> {
    repo: &'a Repository,
    host: &'a str,
    target: &'a Target,
    target_id: Option<&'a str>,
    collector: &'a mut dyn Collector,
}

impl GraphBuilder<'_> {
    fn build(&mut self, host: CollectedUnit) -> Result<DeclarationGraph, GraphError> {
        let mut stack = vec![Pending {
            key: None,
            unit: host,
        }];
        let mut visited = HashMap::new();
        let mut profiles = Vec::new();

        loop {
            let unit = &mut stack.last_mut().expect("声明图包含主机入口").unit;
            let Some(reference) = unit.spec.imports.get(unit.dependencies.len()) else {
                let Pending { key, unit } = stack.pop().expect("声明图包含主机入口");
                let Some(key) = key else {
                    return Ok(DeclarationGraph {
                        host: unit,
                        target: self.target.clone(),
                        profiles,
                    });
                };

                let index = profiles.len();
                profiles.push(unit);
                visited.insert(key, Visit::Complete(index));
                stack
                    .last_mut()
                    .expect("profile 需要导入者")
                    .unit
                    .dependencies
                    .push(index);
                continue;
            };

            let mut params = reference.params.clone();
            for value in params.values_mut() {
                value.sort_all_objects();
            }
            let key = ProfileKey {
                name: reference.name.clone(),
                params: serde_json::to_string(&params).expect("JSON 参数可重新编码"),
            };
            match visited.get(&key) {
                Some(Visit::Active) => {
                    return Err(GraphError::ProfileCycle {
                        trace: import_trace(&stack),
                    });
                }
                Some(Visit::Complete(index)) => {
                    unit.dependencies.push(*index);
                    continue;
                }
                None => {}
            }

            let name = &key.name;
            let profile = self.repo.manifest().profiles.get(name).ok_or_else(|| {
                GraphError::UnknownProfile {
                    name: name.clone(),
                    trace: import_trace(&stack),
                }
            })?;
            let unit = self
                .collect_unit(
                    name,
                    &profile.entry,
                    Priority::Profile,
                    params,
                    profile.inputs,
                )
                .map_err(|source| GraphError::ImportedUnit {
                    trace: import_trace(&stack),
                    source: Box::new(source),
                })?;
            visited.insert(key.clone(), Visit::Active);
            stack.push(Pending {
                key: Some(key),
                unit,
            });
        }
    }

    fn collect_unit(
        &mut self,
        name: &str,
        path: &Path,
        priority: Priority,
        params: BTreeMap<String, Value>,
        input_policy: Option<InputPolicy>,
    ) -> Result<CollectedUnit, GraphError> {
        let entry = canonical_entry(self.repo.root(), path)?;
        let request = CollectRequest {
            repo_root: self.repo.root(),
            entry: &entry,
            name,
            priority,
            params: &params,
            input_policy,
            host: self.host,
            target: self.target,
            target_id: self.target_id,
        };
        let spec = self
            .collector
            .collect(&request)
            .map_err(|source| GraphError::Collect {
                entry: entry.clone(),
                source,
            })?;
        Ok(CollectedUnit {
            name: name.to_owned(),
            entry,
            priority,
            params,
            input_policy,
            target_id: self.target_id.map(str::to_owned),
            spec,
            cache_revision: self.collector.last_cache_revision(),
            dependencies: Vec::new(),
        })
    }
}

fn canonical_entry(root: &Path, entry: &Path) -> Result<String, GraphError> {
    let path = root.join(entry);
    let path = fs::canonicalize(&path).map_err(|source| GraphError::EntryIo { path, source })?;
    let relative = path
        .strip_prefix(root)
        .map_err(|_| GraphError::OutsideRepository(path.clone()))?;
    let parts: Vec<_> = relative
        .iter()
        .map(|part| {
            part.to_str()
                .ok_or_else(|| GraphError::EntryEncoding(path.clone()))
        })
        .collect::<Result<_, _>>()?;
    Ok(parts.join("/"))
}

fn import_trace(stack: &[Pending]) -> String {
    let mut trace = stack.first().expect("声明图包含主机入口").unit.name.clone();
    for pending in stack {
        let unit = &pending.unit;
        let import = &unit.spec.imports[unit.dependencies.len()];
        let location = import
            .origin
            .as_ref()
            .map_or_else(|| unit.entry.clone(), ToString::to_string);
        write!(trace, " -> {} ({location})", import.name).expect("字符串可写");
    }
    trace
}
