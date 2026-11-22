use std::{error::Error, io::Write};

use dek_cli::evaluation::{evaluate_options, expand_host};
use dek_controller::{
    cache::{CacheStore, RefreshHttpInputs},
    eval::{RubyProviders, RubySession, TargetProvider},
    repo::{HostSelector, Repository},
};

pub(super) fn refresh(
    mut out: impl Write,
    repo: &Repository,
    selector: HostSelector<'_>,
    target: &mut dyn TargetProvider,
) -> Result<(), Box<dyn Error>> {
    let cache = CacheStore::for_repository(repo.root())?;
    let mut http = RefreshHttpInputs::new(&cache);
    let mut ruby = RubySession::new();
    let mut providers = RubyProviders {
        target,
        http: Some(&mut http),
    };
    let evaluated = evaluate_options(repo, selector, &mut ruby, providers.reborrow(), None)?;
    expand_host(repo.root(), &evaluated, &mut ruby, providers)?;
    let stats = http.stats();
    writeln!(
        out,
        "主机 {} 已更新 {} 项 HTTP 输入，{} 项内容未变化",
        evaluated.graph.host.name, stats.updated, stats.unchanged
    )?;
    Ok(())
}
