use std::{collections::BTreeMap, path::Path};

use dek_core::{Priority, UnitSpec};
use serde_json::{Value, json};

use crate::{
    cache::{
        CacheDependency, CacheDigest, CacheEntry, CacheError, CacheStore, Publication, UnitKey,
        UnitStage, semantic_digest,
    },
    repo::{InputPolicy, Target},
};

use super::{
    CollectError, CollectRequest, CollectedUnit, Collector, LanguageCollector, observations,
};

#[derive(Clone, Copy)]
enum CachedLanguage {
    Json,
    Ruby,
}

/// Cache eligible declarations while using the ordinary language collector for misses.
pub struct CachedCollector<'cache, 'language> {
    store: &'cache CacheStore,
    language: LanguageCollector<'language>,
    last_revision: Option<CacheDigest>,
}

impl<'cache, 'language> CachedCollector<'cache, 'language> {
    pub fn new(store: &'cache CacheStore, language: LanguageCollector<'language>) -> Self {
        Self {
            store,
            language,
            last_revision: None,
        }
    }

    /// Resolve the expected signature for looking up this unit's stored result.
    /// Ruby signature resolution uses filesystem and environment data.
    pub fn signature_for(&self, unit: &CollectedUnit) -> Option<String> {
        self.signature(cached_language(&unit.entry, unit.input_policy)?)
    }

    fn signature(&self, language: CachedLanguage) -> Option<String> {
        match language {
            CachedLanguage::Json => Some("json-collect-v1".into()),
            CachedLanguage::Ruby => self.language.ruby_signature(),
        }
    }
}

impl Collector for CachedCollector<'_, '_> {
    fn collect(&mut self, request: &CollectRequest<'_>) -> Result<UnitSpec, CollectError> {
        self.last_revision = None;
        let Some(language) = cached_language(request.entry, request.input_policy) else {
            return self.language.collect(request);
        };
        if request.repo_root != self.store.repository_root() {
            return Err(collect_error(
                "declaration cache belongs to another repository",
            ));
        }

        let shared = unit_key(
            request.entry,
            request.name,
            request.priority,
            request.params,
        );
        let mut keys = Vec::new();
        if matches!(language, CachedLanguage::Ruby) {
            keys.push(scoped_key(
                shared.clone(),
                request.host,
                request.target,
                request.target_id,
            ));
        }
        keys.push(shared);

        for _ in 0..2 {
            let Some(signature) = self.signature(language) else {
                return self.language.collect(request);
            };
            // Capture every possible destination before evaluating. The final read set
            // determines whether the result belongs to this host or to the shared key.
            let mut snapshots = keys
                .iter()
                .map(|key| self.store.lookup::<UnitSpec>(key, &signature))
                .collect::<Result<Vec<_>, _>>()
                .map_err(cache_error)?;

            for lookup in &mut snapshots {
                if let Some(entry) = lookup.entry.take()
                    && observations::unchanged(request.repo_root, &entry.dependencies, |input| {
                        self.language.reobserve_provider_input(input)
                    })
                    .unwrap_or(false)
                {
                    self.last_revision = lookup.revision();
                    return Ok(entry.output);
                }
            }

            let spec = self.language.collect(request)?;
            let targeted = spec
                .inputs
                .iter()
                .any(|input| observations::is_target(input.kind));
            let lookup = if targeted {
                snapshots.first().expect("target reads are made by Ruby")
            } else {
                snapshots.last().expect("shared destination")
            };
            let entry = CacheEntry {
                dependencies: spec
                    .inputs
                    .iter()
                    .cloned()
                    .map(CacheDependency::Input)
                    .collect(),
                semantic_digest: collection_semantic_digest(&spec),
                output: spec,
            };
            let (published, revision) = self
                .store
                .publish_with_revision(lookup, &entry, |dependencies| {
                    if self.signature(language).as_deref() != Some(signature.as_str()) {
                        return Ok(false);
                    }
                    observations::unchanged(request.repo_root, dependencies, |input| {
                        self.language.reobserve_provider_input(input)
                    })
                    .map_err(Into::into)
                })
                .map_err(cache_error)?;

            match published {
                Publication::Published => {
                    self.last_revision = revision;
                    return Ok(entry.output);
                }

                Publication::Superseded => continue,
                Publication::InputsChanged => {
                    return Err(collect_error(
                        "inputs changed during declaration collection; retry",
                    ));
                }
            }
        }

        Err(collect_error(
            "declaration cache changed concurrently; retry",
        ))
    }

    fn last_cache_revision(&self) -> Option<CacheDigest> {
        self.last_revision
    }
}

impl CollectedUnit {
    /// Reconstruct the persistent collection key, when this unit uses the cache policy.
    pub fn cache_key(&self, host: &str, target: &Target) -> Option<UnitKey> {
        cached_language(&self.entry, self.input_policy)?;
        Some(self.dependency_key(host, target))
    }

    pub(crate) fn dependency_key(&self, host: &str, target: &Target) -> UnitKey {
        let key = unit_key(&self.entry, &self.name, self.priority, &self.params);
        if self
            .spec
            .inputs
            .iter()
            .any(|input| observations::is_target(input.kind))
        {
            scoped_key(key, host, target, self.target_id.as_deref())
        } else {
            key
        }
    }
}

/// Hash the declarations and their resolution context. Location-only edits
/// preserve the digest. An assignment's file is significant to relative values.
pub fn collection_semantic_digest(spec: &UnitSpec) -> CacheDigest {
    let imports = spec
        .imports
        .iter()
        .map(|entry| (&entry.name, &entry.params))
        .collect::<Vec<_>>();
    let options = spec
        .options
        .iter()
        .map(|option| (&option.path, &option.value, option.origin.file()))
        .collect::<Vec<_>>();
    semantic_digest(&json!([imports, options]))
}

fn cached_language(entry: &str, policy: Option<InputPolicy>) -> Option<CachedLanguage> {
    match Path::new(entry)
        .extension()
        .and_then(|extension| extension.to_str())
    {
        Some("json") => Some(CachedLanguage::Json),
        Some("rb") if policy == Some(InputPolicy::Sdk) => Some(CachedLanguage::Ruby),
        _ => None,
    }
}

fn unit_key(
    entry: &str,
    name: &str,
    priority: Priority,
    params: &BTreeMap<String, Value>,
) -> UnitKey {
    UnitKey {
        stage: UnitStage::Collect,
        unit: json!([priority, name, entry]).to_string(),
        params: params.clone(),
        target: None,
    }
}

fn scoped_key(mut key: UnitKey, host: &str, target: &Target, target_id: Option<&str>) -> UnitKey {
    key.target = Some(target_scope(host, target, target_id));
    key
}

pub(crate) fn target_scope(host: &str, target: &Target, target_id: Option<&str>) -> String {
    match target_id {
        Some(id) => json!([host, target, id]).to_string(),
        None => json!([host, target]).to_string(),
    }
}

fn collect_error(message: impl Into<String>) -> CollectError {
    CollectError {
        origin: None,
        message: message.into(),
    }
}

fn cache_error(error: CacheError) -> CollectError {
    let origin = match &error {
        CacheError::Recheck { source, .. } => source
            .downcast_ref::<CollectError>()
            .and_then(|error| error.origin.clone()),
        _ => None,
    };
    CollectError {
        origin,
        message: error.to_string(),
    }
}
