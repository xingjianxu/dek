use std::collections::BTreeMap;

use dek_core::{Action, Precondition, ServiceEffect, ServiceNotification, UnitId, UnitScope};
use dek_target::backend::{ActionReadiness, BackendError, NotificationError, PlannedNotification};

use super::{ctl, ctl_error, is_running, system_ctl_output, validate_unit};

pub(super) fn validate(unit: &UnitId) -> Result<(), BackendError> {
    validate_unit(unit)?;
    if unit.scope != UnitScope::System || !unit.name.ends_with(".service") {
        return Err(BackendError(format!(
            "服务通知需要系统级 .service 单元 {:?}",
            unit.name
        )));
    }
    Ok(())
}

pub(super) fn plan(
    notice: &ServiceNotification,
    service_will_start: bool,
) -> Result<Option<PlannedNotification>, BackendError> {
    if !service_will_start && is_running(&notice.unit)? != Some(true) {
        return Ok(None);
    }
    let reason = match notice.effect {
        ServiceEffect::Reload => "已更新配置需要服务重新加载",
        ServiceEffect::Restart => "已更新配置需要服务重启",
    };
    Ok(Some(PlannedNotification {
        reason: reason.into(),
        preconditions: vec![running(&notice.unit)],
    }))
}

pub(super) fn inspect(
    action: &Action,
    unit: &UnitId,
    effect: ServiceEffect,
) -> Result<ActionReadiness, BackendError> {
    let output = ctl(
        &UnitScope::System,
        false,
        &[
            "show",
            "--property=LoadState,ActiveState,CanReload,NeedDaemonReload",
            "--",
            &unit.name,
        ],
    )?;
    let fields: BTreeMap<_, _> = output
        .lines()
        .filter_map(|line| line.split_once('='))
        .collect();
    let reason = if action.preconditions != [running(unit)] {
        "服务通知缺少已审阅的运行状态前置条件"
    } else if fields.get("LoadState") != Some(&"loaded")
        || fields.get("ActiveState") != Some(&"active")
    {
        "服务当前未处于已加载的运行状态"
    } else if fields.get("NeedDaemonReload") != Some(&"no") {
        "服务的 systemd 定义需要重新加载，请重新规划"
    } else if effect == ServiceEffect::Reload && fields.get("CanReload") != Some(&"yes") {
        "服务不支持 reload，请检查单元定义"
    } else {
        return Ok(ActionReadiness::Ready);
    };
    Ok(ActionReadiness::Conflict(format!(
        "{}：{reason}",
        unit.name
    )))
}

pub(super) fn execute(unit: &UnitId, effect: ServiceEffect) -> Result<(), NotificationError> {
    let command = match effect {
        ServiceEffect::Reload => "reload",
        ServiceEffect::Restart => "restart",
    };
    let output = system_ctl_output(&[
        "--no-ask-password",
        "--job-mode=fail",
        command,
        "--",
        &unit.name,
    ])
    .map_err(NotificationError::Unverified)?;
    if output.status.success() {
        return Ok(());
    }
    let error = ctl_error(command, &output);
    if output.status.code().is_some() && reports_job_failure(&output.stderr, &unit.name) {
        Err(NotificationError::JobFailed(error))
    } else {
        Err(NotificationError::Unverified(error))
    }
}

fn running(unit: &UnitId) -> Precondition {
    Precondition::UnitRunning {
        unit: unit.clone(),
        expected: true,
    }
}

fn reports_job_failure(stderr: &[u8], unit: &str) -> bool {
    // systemctl prints this report after receiving the matching JobRemoved
    // result. Connection/setup errors also exit nonzero without this report.
    let report = format!("Job for {unit} failed");
    String::from_utf8_lossy(stderr).lines().any(|line| {
        line.strip_prefix(&report)
            .is_some_and(|rest| rest == "." || rest.starts_with(" because "))
    })
}

#[cfg(test)]
mod tests {
    use super::reports_job_failure;

    #[test]
    fn only_a_matching_job_failure_can_be_retried() {
        let unit = "demo.service";
        assert!(reports_job_failure(
            b"Job for demo.service failed.\nSee details.\n",
            unit
        ));
        assert!(reports_job_failure(
            b"Job for demo.service failed because the control process exited with error code.\n",
            unit
        ));
        assert!(!reports_job_failure(
            b"Job for another.service failed.\n",
            unit
        ));
        assert!(!reports_job_failure(
            b"D-Bus connection terminated while waiting for jobs.\n",
            unit
        ));
        assert!(!reports_job_failure(
            b"Job for demo.service canceled.\n",
            unit
        ));
    }
}
