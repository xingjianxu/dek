use std::{error::Error, path::Path};

use dek_controller::{
    cache::{CacheError, CacheStore},
    eval::{
        CachedCollector, EvaluatedHost, HostEvaluationError, LanguageCollector, RubyProviders,
        RubySession, evaluate_host,
    },
    modules::{
        ModuleExpansion, ModuleRegistry, register_app_modules, register_key_modules,
        register_system_modules,
    },
    repo::{HostSelector, Repository},
};

/// The cache is optional for read-only evaluation when the environment has no cache home.
pub fn normal_cache(repo: &Repository) -> Result<Option<CacheStore>, CacheError> {
    match CacheStore::for_repository(repo.root()) {
        Ok(store) => Ok(Some(store)),
        Err(CacheError::CacheHomeUnavailable) => Ok(None),
        Err(error) => Err(error),
    }
}

pub fn evaluate_options(
    repo: &Repository,
    selector: HostSelector<'_>,
    ruby: &mut RubySession,
    providers: RubyProviders<'_>,
    cache: Option<&CacheStore>,
) -> Result<EvaluatedHost, HostEvaluationError> {
    let language = LanguageCollector::new(ruby, providers.target);
    let mut language = match providers.http {
        Some(http) => language.with_http_provider(http),
        None => language,
    };
    match cache {
        Some(cache) => evaluate_host(repo, selector, &mut CachedCollector::new(cache, language)),
        None => evaluate_host(repo, selector, &mut language),
    }
}

pub fn expand_host<'a>(
    repo_root: &Path,
    evaluated: &EvaluatedHost,
    ruby: &'a mut RubySession,
    providers: RubyProviders<'a>,
) -> Result<Vec<ModuleExpansion>, Box<dyn Error>> {
    let mut modules = ModuleRegistry::default();
    register_app_modules(&mut modules)?;
    register_system_modules(&mut modules)?;
    register_key_modules(&mut modules)?;
    evaluated
        .repository_modules
        .register(&mut modules, ruby, providers)?;
    Ok(modules.expand(repo_root, &evaluated.config)?)
}
