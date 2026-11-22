//! Arch package state, native transaction planning, and reviewed package changes.

mod alpm;
mod aur;
mod removal;
mod transaction;
mod upgrade;

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    process::{Command, Output},
};

use dek_core::{
    Action, ActionKind, DesiredResource, PackageId, PackageResource, PackageSource, PlanMode,
    Precondition, Presence, ResourceCapability, ResourceKind, ResourceSet,
};
use dek_target::backend::{
    ActionContext, ActionReadiness, Backend, BackendError, PlannedAction, PlannedPackages,
    RepositoryPreparation,
};

pub struct PacmanBackend {
    program: PathBuf,
}

impl Default for PacmanBackend {
    fn default() -> Self {
        Self {
            program: "/usr/bin/pacman".into(),
        }
    }
}

impl PacmanBackend {
    pub fn available(&self) -> bool {
        self.program.is_file()
    }

    fn output(&self, operation: &str, args: &[&str]) -> Result<Output, BackendError> {
        let output = Command::new(&self.program)
            .args(args)
            .env("LC_ALL", "C")
            .output()
            .map_err(|error| BackendError(format!("无法运行 pacman {operation}：{error}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            // ALPM can forward pre-transaction hook diagnostics on stdout.
            let details = [stderr.trim(), stdout.trim()]
                .into_iter()
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            return Err(BackendError(format!(
                "pacman {operation} 失败（{}）：{details}",
                output.status
            )));
        }
        Ok(output)
    }

    fn installed_packages(&self) -> Result<Output, BackendError> {
        let output = self.output("查询本地软件包", &["--query", "--color", "never"])?;
        let diagnostics = String::from_utf8_lossy(&output.stderr);
        let errors = diagnostics
            .lines()
            .filter(|line| {
                // pacman emits this while initializing configured sync databases,
                // even when the operation only reads the local package database.
                !line.starts_with("warning: database file for '")
                    || !line.ends_with("' does not exist (use '-Sy' to download)")
            })
            .collect::<Vec<_>>();
        if !errors.is_empty() {
            return Err(BackendError(format!(
                "pacman 无法完整读取本地数据库：{}",
                errors.join("\n")
            )));
        }
        Ok(output)
    }

    fn installed(&self, package: &PackageId) -> Result<Option<String>, BackendError> {
        let output = self.installed_packages()?;
        let rows = package_rows(pacman_text(&output)?)?;
        Ok(rows
            .get(package.name.as_str())
            .map(|version| (*version).to_owned()))
    }

    fn preview_removal(&self, package: &PackageId, version: &str) -> Result<(), BackendError> {
        let name = &package.name;
        let output = self.output(
            &format!("预览移除 {name}"),
            &[
                "--remove",
                "--print",
                "--print-format",
                "%n\t%v",
                "--color",
                "never",
                "--",
                name,
            ],
        )?;
        let rows = package_rows(pacman_text(&output)?)?;
        if rows.len() != 1 || rows.get(name.as_str()) != Some(&version) {
            return Err(BackendError(format!(
                "pacman 移除范围或版本与审阅的 {name} {version} 不符"
            )));
        }
        Ok(())
    }
}

impl Backend for PacmanBackend {
    fn capabilities(&self) -> Vec<ResourceCapability> {
        vec![ResourceCapability {
            kind: ResourceKind::Package,
            version: 1,
            attributes: BTreeSet::from(["aur".into()]),
        }]
    }

    fn validate(&self, resource: &DesiredResource) -> Result<(), BackendError> {
        let resource = package_resource(resource)?;
        if resource.package.source == PackageSource::Aur {
            if resource.state == Presence::Absent {
                return Err(BackendError(format!(
                    "当前 pacman 后端无法规划移除 AUR 软件包 {}",
                    resource.package.name
                )));
            }
            return validate_package_name(&resource.package.name);
        }
        validate_package(&resource.package)
    }

    fn observe(&self, resource: &DesiredResource) -> Result<Vec<Precondition>, BackendError> {
        let package = &package_resource(resource)?.package;
        validate_package(package)?;
        Ok(vec![Precondition::PackageVersion {
            package: package.clone(),
            expected: self.installed(package)?,
        }])
    }

    fn plan(
        &self,
        resource: &DesiredResource,
        observed: &[Precondition],
    ) -> Result<Vec<PlannedAction>, BackendError> {
        let resource = package_resource(resource)?;
        validate_package(&resource.package)?;
        let version = package_version(&resource.package, observed)?;
        match (resource.state, version) {
            (Presence::Present, Some(_)) | (Presence::Absent, None) => Ok(Vec::new()),
            (Presence::Present, None) => Err(BackendError(format!(
                "本地未安装软件包 {}。当前后端需要可审阅的仓库候选版本才能规划安装",
                resource.package.name
            ))),
            (Presence::Absent, Some(version)) => {
                self.preview_removal(&resource.package, version)?;
                Ok(vec![PlannedAction {
                    kind: ActionKind::RemovePackage {
                        package: resource.package.clone(),
                    },
                    reason: format!(
                        "已声明移除已安装的软件包 {} {version}",
                        resource.package.name
                    ),
                    preconditions: observed.to_vec(),
                    content_changed: false,
                }])
            }
        }
    }

    fn plan_packages(
        &self,
        resources: &ResourceSet,
        mode: PlanMode,
    ) -> Result<Option<PlannedPackages>, BackendError> {
        self.plan_package_set(resources, mode)
    }

    fn validate_action(&self, action: &Action) -> Result<(), BackendError> {
        match action.kind {
            ActionKind::RepositoryTransaction(_) => {
                transaction::reviewed_install(action).map(|_| ())
            }
            ActionKind::BuildAurPackage { .. } => {
                Err(BackendError("当前后端只支持 AUR 规划".into()))
            }
            _ => reviewed_removal(action).map(|_| ()),
        }
    }

    fn prepare_repository_transaction(
        &self,
        action: &Action,
    ) -> Result<RepositoryPreparation, BackendError> {
        alpm::prepare_install(transaction::reviewed_install(action)?)
    }

    fn inspect_action(&self, action: &Action) -> Result<ActionReadiness, BackendError> {
        if matches!(action.kind, ActionKind::RepositoryTransaction(_)) {
            return self.inspect_install(transaction::reviewed_install(action)?);
        }
        let (package, version) = reviewed_removal(action)?;
        Ok(match self.installed(package)? {
            None => ActionReadiness::Satisfied,
            Some(actual) if actual == version => ActionReadiness::Ready,
            Some(actual) => ActionReadiness::Conflict(format!(
                "软件包 {} 版本已从 {version} 变为 {actual}",
                package.name
            )),
        })
    }

    fn execute(&self, context: &ActionContext<'_>) -> Result<(), BackendError> {
        let (package, version) = reviewed_removal(context.action)?;
        self.preview_removal(package, version)?;
        let guard = removal::prepare(package, version)?;
        let hook_dir = guard
            .path()
            .to_str()
            .ok_or_else(|| BackendError("pacman 事务守卫目录需要 UTF-8 路径".into()))?;
        self.output(
            &format!("移除 {}", package.name),
            &[
                "--remove",
                "--noconfirm",
                "--color",
                "never",
                "--hookdir",
                hook_dir,
                "--",
                &package.name,
            ],
        )?;
        Ok(())
    }

    fn verify(&self, action: &ActionKind) -> Result<bool, BackendError> {
        if let ActionKind::RepositoryTransaction(transaction) = action {
            return self
                .inspect_install(transaction)
                .map(|result| result == ActionReadiness::Satisfied);
        }
        self.installed(removal_package(action)?)
            .map(|version| version.is_none())
    }
}

fn package_resource(resource: &DesiredResource) -> Result<&PackageResource, BackendError> {
    match resource {
        DesiredResource::Package(package) => Ok(package),
        _ => Err(BackendError("pacman 后端需要 Arch 软件包资源".into())),
    }
}

fn package_version<'a>(
    package: &PackageId,
    conditions: &'a [Precondition],
) -> Result<Option<&'a str>, BackendError> {
    match conditions {
        [
            Precondition::PackageVersion {
                package: observed,
                expected,
            },
        ] if observed == package => Ok(expected.as_deref()),
        _ => Err(BackendError(format!(
            "软件包 {} 缺少对应的本地版本前置条件",
            package.name
        ))),
    }
}

fn removal_package(kind: &ActionKind) -> Result<&PackageId, BackendError> {
    match kind {
        ActionKind::RemovePackage { package } => {
            validate_package(package)?;
            Ok(package)
        }
        _ => Err(BackendError(
            "当前 pacman 后端只执行已审阅的软件包移除动作".into(),
        )),
    }
}

fn reviewed_removal(action: &Action) -> Result<(&PackageId, &str), BackendError> {
    let package = removal_package(&action.kind)?;
    let version = package_version(package, &action.preconditions)?
        .filter(|version| valid_version(version))
        .ok_or_else(|| {
            BackendError(format!(
                "软件包 {} 的移除动作需要审阅时的已安装版本",
                package.name
            ))
        })?;
    Ok((package, version))
}

fn validate_package(package: &PackageId) -> Result<(), BackendError> {
    if package.source != PackageSource::Official {
        return Err(BackendError(format!(
            "当前本地 pacman 后端不管理 AUR 软件包 {}",
            package.name
        )));
    }
    validate_package_name(&package.name)
}

fn validate_package_name(name: &str) -> Result<(), BackendError> {
    if !valid_package_name(name) {
        return Err(BackendError(format!("无效 Arch 软件包名 {name:?}")));
    }
    Ok(())
}

fn valid_package_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(['.', '-'])
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"@._+-".contains(&byte))
}

fn valid_version(version: &str) -> bool {
    !version.is_empty() && version.bytes().all(|byte| byte.is_ascii_graphic())
}

fn pacman_text(output: &Output) -> Result<&str, BackendError> {
    std::str::from_utf8(&output.stdout)
        .map_err(|error| BackendError(format!("pacman 软件包列表不是 UTF-8：{error}")))
}

fn package_rows(text: &str) -> Result<BTreeMap<&str, &str>, BackendError> {
    let mut packages = BTreeMap::new();
    for line in text.lines() {
        let fields: Vec<_> = line.split_ascii_whitespace().collect();
        match fields.as_slice() {
            [name, version] if valid_package_name(name) && valid_version(version) => {
                if packages.insert(*name, *version).is_some() {
                    return Err(BackendError(format!("pacman 列出了重复的软件包 {name}")));
                }
            }
            _ => return Err(BackendError(format!("无法解析 pacman 软件包行 {line:?}"))),
        }
    }
    Ok(packages)
}

#[cfg(test)]
mod tests;
