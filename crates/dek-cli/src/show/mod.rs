use std::{collections::BTreeMap, error::Error, io::Write};

use crate::evaluation::evaluate_options;
use dek_controller::{
    cache::{CacheStore, CachedHttpInputs},
    eval::{HttpInputProvider, RubyProviders, RubySession, TargetAnswer, TargetProvider},
    repo::{HostSelector, Repository},
};
use dek_core::{InputKind, OptionPath, Priority, protocol::TargetQuery};
use serde_json::json;

pub(super) struct UnavailableTarget;

impl TargetProvider for UnavailableTarget {
    fn query(&mut self, query: &TargetQuery) -> Result<TargetAnswer, String> {
        Err(format!(
            "当前未配置目标查询提供者，无法读取 {:?}",
            query.key
        ))
    }
}

pub(super) fn show(
    mut out: impl Write,
    repo: &Repository,
    selector: HostSelector<'_>,
    path: Option<&OptionPath>,
    as_json: bool,
    target_provider: &mut dyn TargetProvider,
    cache: Option<&CacheStore>,
) -> Result<(), Box<dyn Error>> {
    let mut http = cache.map(CachedHttpInputs::new);
    let providers = RubyProviders {
        target: target_provider,
        http: http
            .as_mut()
            .map(|inputs| inputs as &mut dyn HttpInputProvider),
    };
    let evaluated = evaluate_options(repo, selector, &mut RubySession::new(), providers, cache)?;
    let options: BTreeMap<_, _> = if let Some(path) = path {
        let pair = evaluated
            .config
            .options
            .get_key_value(path)
            .ok_or_else(|| format!("找不到生效选项 {path}"))?;
        BTreeMap::from([pair])
    } else {
        evaluated.config.options.iter().collect()
    };
    let graph = evaluated.graph;
    let queried_target = graph
        .profiles
        .iter()
        .chain(std::iter::once(&graph.host))
        .flat_map(|unit| &unit.spec.inputs)
        .any(|input| {
            matches!(
                input.kind,
                InputKind::TargetFact | InputKind::TargetCapability | InputKind::TargetFile
            )
        });
    let snapshot = if queried_target {
        target_provider.snapshot_source()
    } else {
        None
    };

    if as_json {
        let mut document = json!({
            "host": graph.host.name, "target": graph.target, "options": options
        });
        if let Some(snapshot) = snapshot {
            document["targetSnapshot"] = json!(snapshot);
        }
        serde_json::to_writer_pretty(&mut out, &document)?;
        writeln!(out)?;
        return Ok(());
    }

    writeln!(out, "主机 {}", graph.host.name)?;
    if let Some(snapshot) = snapshot {
        writeln!(out, "目标快照 {}", snapshot.label)?;
    }
    for (name, option) in options {
        writeln!(out, "{name} = {}", option.value)?;
        if path.is_some() {
            writeln!(out, "  优先级 {}", priority_name(option.priority))?;
            for definition in &option.effective_definitions {
                writeln!(
                    out,
                    "  生效 {} [{}] {}",
                    definition.value,
                    priority_name(definition.priority),
                    definition.origin
                )?;
            }
            for definition in &option.overridden_definitions {
                writeln!(
                    out,
                    "  被覆盖 {} [{}] {}",
                    definition.value,
                    priority_name(definition.priority),
                    definition.origin
                )?;
            }
        }
    }
    Ok(())
}

fn priority_name(priority: Priority) -> &'static str {
    match priority {
        Priority::ModuleDefault => "模块默认",
        Priority::Profile => "profile",
        Priority::Host => "主机",
    }
}

#[cfg(test)]
mod tests;
