use std::{error::Error, io::Write};

use dek_core::{
    ActionKind, PendingRunNotification, Plan, RepositoryPackageChange, RepositoryTransaction,
    RepositoryTransactionScope, ServiceEffect, UnitId, UnitScope,
};
use serde_json::json;

pub fn write_plan(
    mut out: impl Write,
    host: &str,
    plan: &Plan,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    if json {
        serde_json::to_writer_pretty(&mut out, &plan_document(host, plan)?)?;
        writeln!(out)?;
        return Ok(());
    }

    writeln!(out, "主机 {host}（目标 {}）", plan.target.id)?;
    writeln!(out, "计划 {}", plan.id)?;
    writeln!(out)?;
    if plan.actions.is_empty() {
        writeln!(out, "已符合声明，0 项变更")?;
    }
    for action in &plan.actions {
        let label = description(&action.kind);
        if let Some((title, details)) = label.split_once('\n') {
            writeln!(out, "{title}  [{}]\n{details}", action.id)?;
        } else {
            writeln!(out, "{label}  [{}]", action.id)?;
        }
        if let ActionKind::BuildAurPackage { dependencies, .. }
        | ActionKind::RebuildAurPackage { dependencies, .. } = &action.kind
        {
            for (label, packages) in [
                ("运行依赖", &dependencies.runtime),
                ("构建依赖", &dependencies.build),
                ("检查依赖", &dependencies.check),
            ] {
                if !packages.is_empty() {
                    writeln!(out, "  {label} {}", packages.join(", "))?;
                }
            }
        }
        writeln!(out, "  原因 {}", action.reason)?;
        if !action.depends_on.is_empty() {
            writeln!(out, "  依赖 {}", action.depends_on.join(", "))?;
        }
    }
    Ok(())
}

pub(crate) fn plan_document(
    host: &str,
    plan: &Plan,
) -> Result<serde_json::Value, serde_json::Error> {
    let mut value = serde_json::to_value(plan)?;
    let document = value
        .as_object_mut()
        .expect("core plan serializes as an object");
    document.remove("id");
    document.insert("plan_id".into(), plan.id.as_str().into());
    document.insert("host".into(), host.into());
    Ok(value)
}

pub(crate) fn description(action: &ActionKind) -> String {
    match action {
        ActionKind::WriteFile { path, .. } => format!("更新文件 {path}"),
        ActionKind::SetFileAttributes { path, .. } => format!("调整文件属性 {path}"),
        ActionKind::RemoveFile { path } => format!("移除文件 {path}"),
        ActionKind::EnsureDirectory(directory) => format!("设置目录 {}", directory.path),
        ActionKind::EnsureUser(user) => format!("设置用户 {}", user.name),
        ActionKind::SetUserPrimaryGroup { user, group } => {
            format!("设置用户 {user} 的主组 {group}")
        }
        ActionKind::EnsureGroup(group) => format!("设置组 {}", group.name),
        ActionKind::InstallPackage {
            package,
            candidate_version,
        } => {
            format!("安装软件包 {} ({candidate_version})", package.name)
        }
        ActionKind::RemovePackage { package } => format!("移除软件包 {}", package.name),
        ActionKind::RepositoryTransaction(transaction) => describe_transaction(transaction),
        ActionKind::BuildAurPackage {
            package,
            candidate_version,
            ..
        } => format!(
            "构建 AUR 软件包 {} ({})",
            package.name,
            candidate_version.as_deref().unwrap_or("候选版本待核查"),
        ),
        ActionKind::RebuildAurPackage {
            package,
            candidate_version,
            ..
        } => format!("重建 AUR 软件包 {} ({candidate_version})", package.name),
        ActionKind::DeployUnit { unit, .. } => format!("部署 unit {}", unit_name(unit)),
        ActionKind::ReloadManager(scope) => format!("重载 systemd manager {}", scope_name(scope)),
        ActionKind::SetUnitEnabled { unit, enabled } => {
            let verb = if *enabled {
                "启用自启"
            } else {
                "禁用自启"
            };
            format!("{verb} {}", unit_name(unit))
        }
        ActionKind::SetUnitRunning { unit, running } => {
            let verb = if *running {
                "启动 unit"
            } else {
                "停止 unit"
            };
            format!("{verb} {}", unit_name(unit))
        }
        ActionKind::NotifyService { unit, effect, .. } => {
            let verb = match effect {
                ServiceEffect::Reload => "重载服务",
                ServiceEffect::Restart => "重启服务",
            };
            format!("{verb} {}", unit_name(unit))
        }
        ActionKind::UpdateAuthorizedKeys { keys, home, .. } => {
            format!("更新 {} 的授权公钥 {home}/.ssh/authorized_keys", keys.user)
        }
        ActionKind::DeployAgeSecret(secret) => {
            format!("部署加密文件 {} ({})", secret.path, secret.ciphertext.0)
        }
        ActionKind::SetAgeSecretAttributes(secret) => {
            format!("调整加密文件属性 {} ({})", secret.path, secret.ciphertext.0)
        }
    }
}

fn describe_transaction(transaction: &RepositoryTransaction) -> String {
    let title = match transaction.scope {
        RepositoryTransactionScope::Install => "仓库软件包补装事务",
        RepositoryTransactionScope::Upgrade => "完整仓库升级事务",
    };
    let mut lines = vec![title.to_owned()];
    for change in &transaction.changes {
        lines.push(match change {
            RepositoryPackageChange::Install { candidate } => format!(
                "  新增 {}/{} {}",
                candidate.repository, candidate.name, candidate.version
            ),
            RepositoryPackageChange::Upgrade {
                installed_version,
                candidate,
            } => format!(
                "  升级 {}/{} {} → {}",
                candidate.repository, candidate.name, installed_version, candidate.version
            ),
            RepositoryPackageChange::Replace {
                package,
                installed_version,
                candidate,
            } => format!(
                "  替换 {package} {installed_version} → {}/{} {}",
                candidate.repository, candidate.name, candidate.version
            ),
            RepositoryPackageChange::Remove {
                package,
                installed_version,
            } => format!("  移除 {package} {installed_version}"),
        });
    }
    lines.join("\n")
}

fn unit_name(unit: &UnitId) -> String {
    match unit.scope {
        UnitScope::System => unit.name.clone(),
        _ => format!("{} ({})", unit.name, scope_name(&unit.scope)),
    }
}

fn scope_name(scope: &UnitScope) -> &str {
    match scope {
        UnitScope::System => "系统级",
        UnitScope::UserGlobal => "全局用户级",
        UnitScope::User { user } => user,
    }
}

pub(crate) fn write_plan_with_pending(
    mut out: impl Write,
    host: &str,
    plan: &Plan,
    json_output: bool,
    pending: &[PendingRunNotification],
) -> Result<(), Box<dyn Error>> {
    if pending.is_empty() {
        return write_plan(out, host, plan, json_output);
    }
    if json_output {
        let mut document = plan_document(host, plan)?;
        document["pending_notifications"] = json!(pending);
        serde_json::to_writer_pretty(&mut out, &document)?;
        writeln!(out)?;
        return Ok(());
    }
    write_plan(&mut out, host, plan, false)?;
    for item in pending {
        writeln!(
            out,
            "待续办 [{}] {}，执行 {}",
            item.notification.action_id,
            description(&ActionKind::NotifyService {
                unit: item.notification.unit.clone(),
                effect: item.notification.effect,
                sources: Vec::new(),
            }),
            item.run_id,
        )?;
    }
    Ok(())
}
