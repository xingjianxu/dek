use std::collections::BTreeSet;

use dek_core::{
    Action, ActionKind, PackageSource, Precondition, RepositoryPackageChange,
    RepositoryTransaction, RepositoryTransactionScope,
};
use dek_target::backend::{ActionReadiness, BackendError};

use super::{PacmanBackend, package_rows, pacman_text, valid_package_name, valid_version};

pub(super) fn reviewed_install(action: &Action) -> Result<&RepositoryTransaction, BackendError> {
    let ActionKind::RepositoryTransaction(transaction) = &action.kind else {
        return Err(BackendError("需要已审阅的仓库补装事务".into()));
    };
    if transaction.scope != RepositoryTransactionScope::Install {
        return Err(BackendError("完整仓库升级仍需经确认的升级执行器".into()));
    }
    let mut names = BTreeSet::new();
    for change in &transaction.changes {
        let RepositoryPackageChange::Install { candidate } = change else {
            return Err(BackendError("独立补装事务包含对已安装软件包的变更".into()));
        };
        if !valid_package_name(&candidate.name)
            || !valid_version(&candidate.version)
            || !valid_package_name(&candidate.repository)
            || candidate.sha256.len() != 64
            || !candidate
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || !names.insert(candidate.name.as_str())
        {
            return Err(BackendError(format!(
                "补装事务包含无效或重复的仓库候选 {}",
                candidate.name
            )));
        }
    }
    let targets: BTreeSet<_> = transaction.targets.iter().map(String::as_str).collect();
    if targets.is_empty()
        || targets.len() != transaction.targets.len()
        || !targets.is_subset(&names)
    {
        return Err(BackendError("补装事务缺少对应的请求软件包".into()));
    }
    let missing: BTreeSet<_> = action
        .preconditions
        .iter()
        .filter_map(|condition| match condition {
            Precondition::PackageVersion {
                package,
                expected: None,
            } if package.source == PackageSource::Official => Some(package.name.as_str()),
            _ => None,
        })
        .collect();
    if missing != names || action.preconditions.len() != names.len() {
        return Err(BackendError("补装事务缺少软件包未安装的前置条件".into()));
    }
    Ok(transaction)
}

impl PacmanBackend {
    pub(super) fn inspect_install(
        &self,
        transaction: &RepositoryTransaction,
    ) -> Result<ActionReadiness, BackendError> {
        let output = self.installed_packages()?;
        let installed = package_rows(pacman_text(&output)?)?;
        let mut present = 0;
        for change in &transaction.changes {
            let RepositoryPackageChange::Install { candidate } = change else {
                return Err(BackendError("核对补装时遇到已安装软件包的变更".into()));
            };
            match installed.get(candidate.name.as_str()) {
                Some(version) if *version == candidate.version => present += 1,
                Some(version) => {
                    return Ok(ActionReadiness::Conflict(format!(
                        "软件包 {} 当前版本 {version} 与审阅候选 {} 不符",
                        candidate.name, candidate.version
                    )));
                }
                None => {}
            }
        }
        Ok(if present == transaction.changes.len() {
            ActionReadiness::Satisfied
        } else if present == 0 {
            ActionReadiness::Ready
        } else {
            ActionReadiness::Conflict("只有部分已审阅软件包存在，请重新规划".into())
        })
    }
}
