use std::collections::{BTreeMap, BTreeSet};

use dek_core::{
    Action, ActionKind, PackageSource, Precondition, RepositoryPackageChange,
    RepositoryTransaction, RepositoryTransactionScope,
};
use dek_target::backend::{ActionReadiness, BackendError};

use super::{PacmanBackend, package_rows, pacman_text, valid_package_name, valid_version};

pub(super) fn reviewed_transaction(
    action: &Action,
) -> Result<&RepositoryTransaction, BackendError> {
    let ActionKind::RepositoryTransaction(transaction) = &action.kind else {
        return Err(BackendError("需要已审阅的仓库事务".into()));
    };
    let versions = versions(transaction)?;
    let targets: BTreeSet<_> = transaction.targets.iter().map(String::as_str).collect();
    if (targets.is_empty() && transaction.scope == RepositoryTransactionScope::Install)
        || targets.len() != transaction.targets.len()
        || targets
            .iter()
            .any(|name| versions.before.get(name) != Some(&None))
    {
        return Err(BackendError("仓库事务缺少对应的请求软件包".into()));
    }
    if action.preconditions.len() != versions.before.len()
        || action
            .preconditions
            .iter()
            .filter_map(|condition| match condition {
                Precondition::PackageVersion { package, expected }
                    if package.source == PackageSource::Official =>
                {
                    Some((package.name.as_str(), expected.as_deref()))
                }
                _ => None,
            })
            .collect::<BTreeMap<_, _>>()
            != versions.before
    {
        return Err(BackendError(
            "仓库事务的本地版本前置条件与审阅清单不符".into(),
        ));
    }
    Ok(transaction)
}

#[derive(Default)]
struct Versions<'a> {
    before: BTreeMap<&'a str, Option<&'a str>>,
    after: BTreeMap<&'a str, Option<&'a str>>,
}

fn versions(transaction: &RepositoryTransaction) -> Result<Versions<'_>, BackendError> {
    let mut result = Versions::default();
    let mut candidates = BTreeMap::new();
    for change in &transaction.changes {
        if transaction.scope == RepositoryTransactionScope::Install
            && !matches!(change, RepositoryPackageChange::Install { .. })
        {
            return Err(BackendError("独立补装事务包含对已安装软件包的变更".into()));
        }
        let candidate = match change {
            RepositoryPackageChange::Install { candidate } => {
                record_previous(&mut result, &candidate.name, None)?;
                candidate
            }
            RepositoryPackageChange::Upgrade {
                installed_version,
                candidate,
            } => {
                record_previous(&mut result, &candidate.name, Some(installed_version))?;
                candidate
            }
            RepositoryPackageChange::Replace {
                package,
                installed_version,
                candidate,
            } => {
                record_previous(&mut result, package, Some(installed_version))?;
                result.after.insert(package.as_str(), None);
                candidate
            }
            RepositoryPackageChange::Remove {
                package,
                installed_version,
            } => {
                record_previous(&mut result, package, Some(installed_version))?;
                result.after.insert(package.as_str(), None);
                continue;
            }
        };
        if !valid_package_name(&candidate.name)
            || !valid_version(&candidate.version)
            || !valid_package_name(&candidate.repository)
            || candidate.sha256.len() != 64
            || !candidate
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || candidates
                .insert(candidate.name.as_str(), candidate)
                .is_some_and(|old| old != candidate)
        {
            return Err(BackendError(format!(
                "仓库事务包含无效或不一致的候选 {}",
                candidate.name
            )));
        }
    }
    for (name, candidate) in candidates {
        result.before.entry(name).or_insert(None);
        if result
            .after
            .insert(name, Some(&candidate.version))
            .is_some()
        {
            return Err(BackendError(format!("仓库事务同时移除和安装 {name}")));
        }
    }
    Ok(result)
}

fn record_previous<'a>(
    versions: &mut Versions<'a>,
    name: &'a str,
    installed: Option<&'a str>,
) -> Result<(), BackendError> {
    if !valid_package_name(name)
        || installed.is_some_and(|version| !valid_version(version))
        || versions.before.insert(name, installed).is_some()
    {
        return Err(BackendError(format!(
            "仓库事务包含无效或重复的软件包变更 {name}"
        )));
    }
    Ok(())
}

impl PacmanBackend {
    pub(super) fn inspect_repository(
        &self,
        transaction: &RepositoryTransaction,
    ) -> Result<ActionReadiness, BackendError> {
        let versions = versions(transaction)?;
        let output = self.installed_packages()?;
        let installed = package_rows(pacman_text(&output)?)?;
        let matches = |expected: &BTreeMap<&str, Option<&str>>| {
            expected
                .iter()
                .all(|(name, expected)| installed.get(name).copied() == *expected)
        };
        Ok(if matches(&versions.after) {
            self.inspect_installed_records(
                versions
                    .after
                    .iter()
                    .filter_map(|(&name, version)| version.map(|_| name)),
            )?;
            ActionReadiness::Satisfied
        } else if matches(&versions.before) {
            ActionReadiness::Ready
        } else {
            ActionReadiness::Conflict("本机包数据库与审阅前后的完整事务均不符，请重新规划".into())
        })
    }
}
