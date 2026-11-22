//! Present the full upgrade and require an explicit terminal confirmation.

use std::{
    error::Error,
    io::{self, Write},
    path::Path,
};

use dek_controller::repo::{HostSelector, Repository};
use dek_core::{Plan, PlanMode};
use dek_target::{backend::BackendRegistry, state::StateStore};

use crate::{
    applying::{ApplySelection, apply_local, create_plan},
    plan_output::write_plan,
};

pub(crate) fn upgrade_local(
    out: &mut impl Write,
    repo: &Repository,
    name: &str,
    registry: &BackendRegistry<'_>,
    state_dir: &Path,
    interactive: bool,
) -> Result<(), Box<dyn Error>> {
    let host = HostSelector::Named(name);
    let store = StateStore::open(state_dir)?;
    let plan = create_plan(repo, host, registry, &store, PlanMode::Upgrade)?;
    if confirm(out, name, &plan, interactive)? {
        apply_local(
            out,
            repo,
            host,
            registry,
            state_dir,
            ApplySelection {
                plan: Some(&plan.id),
                ..ApplySelection::default()
            },
        )?;
    }
    Ok(())
}

pub(crate) fn confirm(
    out: &mut impl Write,
    host: &str,
    plan: &Plan,
    interactive: bool,
) -> Result<bool, Box<dyn Error>> {
    write_plan(&mut *out, host, plan, false)?;
    if !interactive {
        writeln!(
            out,
            "请审阅升级计划并运行 dek apply -H {host} --plan {}",
            plan.id
        )?;
        out.flush()?;
        return Ok(false);
    }
    write!(out, "执行已审阅的升级计划？[y/N] ")?;
    out.flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    let answer = answer.trim();
    if answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes") {
        return Ok(true);
    }
    writeln!(out, "已取消升级")?;
    Ok(false)
}
