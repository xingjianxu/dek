use std::collections::BTreeSet;

use dek_core::{
    ActionKind, DesiredResource, PackageId, PackageSource, PlanMode, Precondition, Presence,
    RepositoryPackageChange, RepositoryTransaction, ResourceId, ResourceSet,
};
use dek_target::backend::{Backend, BackendError, PlannedAction, PlannedPackages};

use super::{PacmanBackend, alpm, aur, package_rows, pacman_text};

impl PacmanBackend {
    pub(super) fn plan_package_set(
        &self,
        resources: &ResourceSet,
        mode: PlanMode,
    ) -> Result<Option<PlannedPackages>, BackendError> {
        let packages = resources
            .resources
            .iter()
            .filter_map(|resource| {
                if let DesiredResource::Package(package) = &resource.desired {
                    Some(package)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        if packages.is_empty() && mode == PlanMode::Normal {
            return Ok(None);
        }
        let output = self.installed_packages()?;
        let installed = package_rows(pacman_text(&output)?)?;
        let targets = packages
            .iter()
            .filter(|desired| {
                desired.package.source == PackageSource::Official
                    && desired.state == Presence::Present
                    && !installed.contains_key(desired.package.name.as_str())
            })
            .map(|desired| desired.package.name.clone())
            .collect::<BTreeSet<_>>();
        let resolved = if targets.is_empty() && mode == PlanMode::Normal {
            None
        } else {
            let targets = targets.into_iter().collect::<Vec<_>>();
            let requested = targets.join(", ");
            Some(
                alpm::preview(&installed, targets, mode == PlanMode::Upgrade).map_err(|error| {
                    if requested.is_empty() {
                        error
                    } else {
                        BackendError(format!("本地未安装软件包 {requested}。{error}"))
                    }
                })?,
            )
        };
        let mut planned = PlannedPackages::default();
        let (adding, removing) = resolved
            .as_ref()
            .map(|(tx, _)| impact(tx))
            .unwrap_or_default();
        let full = resolved
            .as_ref()
            .is_some_and(|(tx, _)| tx.scope == dek_core::RepositoryTransactionScope::Upgrade);
        let aur_queries = packages
            .iter()
            .filter_map(|desired| {
                let name = desired.package.name.as_str();
                (desired.package.source == PackageSource::Aur
                    && desired.state == Presence::Present
                    && (full || !installed.contains_key(name)))
                .then_some(name)
            })
            .collect::<BTreeSet<_>>();
        let mut aur_candidates = if aur_queries.is_empty() {
            Default::default()
        } else {
            aur::BuildEnvironment::discover()
                .and_then(|environment| environment.candidates(&aur_queries))
                .map_err(|error| {
                    BackendError(format!(
                        "受管 AUR 包 {}：{error}",
                        aur_queries.iter().copied().collect::<Vec<_>>().join(", ")
                    ))
                })?
        };
        for desired in packages {
            let package = &desired.package;
            let name = package.name.as_str();
            let id = ResourceId::Package(package.clone());
            let version = installed.get(name).copied();
            let preconditions = vec![version_condition(package.source, name, version)];
            let actions = match (package.source, desired.state) {
                (PackageSource::Official, Presence::Present)
                    if removing.contains(name) && !adding.contains(name) =>
                {
                    return Err(BackendError(format!(
                        "仓库事务会移除仍声明为已安装的软件包 {name}"
                    )));
                }
                (PackageSource::Official, Presence::Absent) if adding.contains(name) => {
                    return Err(BackendError(format!(
                        "仓库事务会安装或升级已声明移除的软件包 {name}，请先独立处理移除"
                    )));
                }
                (PackageSource::Official, _)
                    if adding.contains(name) || removing.contains(name) =>
                {
                    planned.covered.insert(id);
                    continue;
                }
                (PackageSource::Official, _) => {
                    self.plan(&DesiredResource::Package(desired.clone()), &preconditions)?
                }
                (PackageSource::Aur, Presence::Present) => {
                    let Some(candidate) = aur_candidates.remove(name) else {
                        planned.actions.insert(id, Vec::new());
                        continue;
                    };
                    if let Some(version) = version
                        && !aur::installed_is_older(version, &candidate.version)?
                    {
                        planned.actions.insert(id, Vec::new());
                        continue;
                    }
                    let reason = match version {
                        Some(version) => format!(
                            "受管 AUR 包 {name} 已安装 {version}，候选为 {}",
                            candidate.version
                        ),
                        None => format!("受管 AUR 包 {name} 尚未安装"),
                    };
                    vec![PlannedAction {
                        kind: ActionKind::BuildAurPackage {
                            package: package.clone(),
                            candidate_version: Some(candidate.version),
                            dependencies: candidate.dependencies,
                        },
                        reason,
                        preconditions,
                        content_changed: false,
                    }]
                }
                (PackageSource::Aur, _) => Vec::new(),
            };
            planned.actions.insert(id, actions);
        }
        let preconditions = adding
            .union(&removing)
            .map(|name| {
                version_condition(PackageSource::Official, name, installed.get(name).copied())
            })
            .collect();
        if let Some((transaction, reason)) = resolved {
            planned.transaction = Some(PlannedAction {
                kind: ActionKind::RepositoryTransaction(transaction),
                reason,
                preconditions,
                content_changed: false,
            });
        }
        Ok(Some(planned))
    }
}

fn impact(transaction: &RepositoryTransaction) -> (BTreeSet<&str>, BTreeSet<&str>) {
    let (mut adding, mut removing) = (BTreeSet::new(), BTreeSet::new());
    for change in &transaction.changes {
        match change {
            RepositoryPackageChange::Install { candidate }
            | RepositoryPackageChange::Upgrade { candidate, .. } => {
                adding.insert(candidate.name.as_str());
            }
            RepositoryPackageChange::Replace {
                package, candidate, ..
            } => {
                adding.insert(candidate.name.as_str());
                removing.insert(package.as_str());
            }
            RepositoryPackageChange::Remove { package, .. } => {
                removing.insert(package.as_str());
            }
        }
    }
    (adding, removing)
}

fn version_condition(source: PackageSource, name: &str, version: Option<&str>) -> Precondition {
    Precondition::PackageVersion {
        package: PackageId {
            source,
            name: name.into(),
        },
        expected: version.map(str::to_owned),
    }
}
