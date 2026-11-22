use std::{error::Error, io::Write};

use dek_core::{ActionKind, Plan, ServiceEffect, UnitId, UnitScope};

pub fn write_plan(
    mut out: impl Write,
    host: &str,
    plan: &Plan,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    if json {
        let mut value = serde_json::to_value(plan)?;
        let document = value
            .as_object_mut()
            .expect("core plan serializes as an object");
        document.remove("id");
        document.insert("plan_id".into(), plan.id.as_str().into());
        document.insert("host".into(), host.into());
        serde_json::to_writer_pretty(&mut out, &value)?;
        writeln!(out)?;
        return Ok(());
    }

    writeln!(out, "主机 {host}（本机 {}）", plan.target.id)?;
    writeln!(out, "计划 {}", plan.id)?;
    writeln!(out)?;
    if plan.actions.is_empty() {
        writeln!(out, "已符合声明，0 项变更")?;
    }
    for action in &plan.actions {
        writeln!(out, "{}  [{}]", description(&action.kind), action.id)?;
        writeln!(out, "  原因 {}", action.reason)?;
        if !action.depends_on.is_empty() {
            writeln!(out, "  依赖 {}", action.depends_on.join(", "))?;
        }
    }
    Ok(())
}

fn description(action: &ActionKind) -> String {
    match action {
        ActionKind::WriteFile { path, .. } => format!("更新文件 {path}"),
        ActionKind::SetFileAttributes { path, .. } => format!("调整文件属性 {path}"),
        ActionKind::RemoveFile { path } => format!("移除文件 {path}"),
        ActionKind::EnsureDirectory(directory) => format!("设置目录 {}", directory.path),
        ActionKind::EnsureUser(user) => format!("设置用户 {}", user.name),
        ActionKind::EnsureGroup(group) => format!("设置组 {}", group.name),
        ActionKind::InstallPackage {
            package,
            candidate_version,
        } => {
            format!("安装软件包 {} ({candidate_version})", package.name)
        }
        ActionKind::RemovePackage { package } => format!("移除软件包 {}", package.name),
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
        ActionKind::NotifyService { unit, effect } => {
            let verb = match effect {
                ServiceEffect::Reload => "重载服务",
                ServiceEffect::Restart => "重启服务",
            };
            format!("{verb} {}", unit_name(unit))
        }
        ActionKind::UpdateAuthorizedKeys(keys) => format!("更新授权公钥 {}", keys.user),
        ActionKind::DeployAgeSecret(secret) => format!("部署加密文件 {}", secret.path),
    }
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
