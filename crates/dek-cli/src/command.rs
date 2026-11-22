use std::{error::Error, ffi::OsString, io::Write, path::PathBuf};

use crate::{
    evaluation::normal_cache, local_target::LocalEvaluationTarget, plan_output::write_plan,
    planning::plan_host_with_cache,
};
use clap::{Parser, Subcommand, ValueEnum};
use dek_controller::repo::{EntryLanguage, HostSelector, Repository, Target, initialize};
use dek_core::OptionPath;
use serde_json::json;

use crate::{
    refresh,
    repository::{HostSelection, discover_repo, local_hostname},
    show,
};
use dek_target::backend::BackendRegistry;

#[derive(Parser)]
#[command(name = "dek", version, about = "声明式主机配置管理器")]
struct Cli {
    /// 显式指定配置仓库目录。
    #[arg(long, global = true, value_name = "目录")]
    repo: Option<PathBuf>,

    /// 选择清单中的主机名称。
    #[arg(short = 'H', long, global = true, value_name = "主机")]
    host: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 初始化当前主机的配置仓库。
    Init {
        /// 初始化目录，默认为当前目录。
        #[arg(value_name = "目录")]
        directory: Option<PathBuf>,
        /// 主机入口的配置语言。
        #[arg(long, value_enum, default_value = "json")]
        lang: Language,
    },
    /// 列出清单中的主机和连接方式。
    Hosts {
        /// 输出 JSON。
        #[arg(long)]
        json: bool,
    },
    /// 刷新选定主机显式使用的 HTTP 输入。
    Refresh,
    /// 查看生效配置和选项定义来源。
    Show {
        /// 要追溯的完整选项路径，省略时显示整机配置。
        #[arg(value_name = "选项")]
        option: Option<OptionPath>,
        /// 输出 JSON。
        #[arg(long)]
        json: bool,
    },
    /// 观测本机目标并预览配置差异。
    Plan {
        /// 使用指定的私有目标状态目录。默认 /var/lib/dek。
        #[cfg(target_os = "linux")]
        #[arg(long, value_name = "目录")]
        state_dir: Option<PathBuf>,
        /// 输出 JSON。
        #[arg(long)]
        json: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Language {
    Json,
    Ruby,
}

/// Parse a dek invocation and run it with the local target registry supplied by the caller.
pub fn run<'target>(
    args: impl IntoIterator<Item = impl Into<OsString> + Clone>,
    mut out: impl Write,
    local_target: impl FnOnce() -> Result<BackendRegistry<'target>, Box<dyn Error>>,
) -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse_from(args);
    match cli.command {
        Command::Init { directory, lang } => {
            if cli.host.is_some() {
                return Err("dek init 始终使用当前主机名，请省略 --host".into());
            }
            if directory.is_some() && cli.repo.is_some() {
                return Err("初始化目录只能通过位置参数或 --repo 指定一次".into());
            }
            let root = directory.or(cli.repo).unwrap_or_else(|| PathBuf::from("."));
            let hostname = local_hostname()?;
            let language = match lang {
                Language::Json => EntryLanguage::Json,
                Language::Ruby => EntryLanguage::Ruby,
            };
            initialize(&root, &hostname, language)?;
            writeln!(out, "已初始化配置仓库 {}", root.display())?;
        }
        Command::Hosts { json } => {
            let repo = discover_repo(cli.repo.as_deref())?;
            list_hosts(&mut out, &repo, cli.host.as_deref(), json)?;
        }
        Command::Show { option, json } => {
            let repo = discover_repo(cli.repo.as_deref())?;
            let selected = HostSelection::from_arg(cli.host)?;
            let cache = normal_cache(&repo)?;
            show::show(
                &mut out,
                &repo,
                selected.selector(),
                option.as_ref(),
                json,
                &mut show::UnavailableTarget,
                cache.as_ref(),
            )?;
        }
        Command::Refresh => {
            let repo = discover_repo(cli.repo.as_deref())?;
            let selected = HostSelection::from_arg(cli.host)?;
            let selector = selected.selector();
            if repo.select_host(selector)?.1.target == Target::Local {
                let registry = local_target()?;
                let mut provider = LocalEvaluationTarget(&registry);
                refresh::refresh(&mut out, &repo, selector, &mut provider)?;
            } else {
                refresh::refresh(&mut out, &repo, selector, &mut show::UnavailableTarget)?;
            }
        }
        Command::Plan {
            json,
            #[cfg(target_os = "linux")]
            state_dir,
        } => {
            let repo = discover_repo(cli.repo.as_deref())?;
            let selection = HostSelection::from_arg(cli.host)?;
            let registry = local_target()?;
            let mut provider = LocalEvaluationTarget(&registry);
            let cache = normal_cache(&repo)?;
            let planned = plan_host_with_cache(
                &repo,
                selection.selector(),
                &registry,
                &mut provider,
                cache.as_ref(),
            )?;
            #[cfg(target_os = "linux")]
            {
                use dek_target::state::{DEFAULT_ROOT, StateStore};

                let state_dir = state_dir.unwrap_or_else(|| PathBuf::from(DEFAULT_ROOT));
                let store = StateStore::open(&state_dir).map_err(|err| {
                    format!(
                        "{err}。请使用持有目标状态目录的账户，或用 --state-dir 指定该账户的私有目录"
                    )
                })?;
                store.lock()?.save_plan(
                    &planned.name,
                    &planned.plan,
                    &planned.expanded.resources,
                    &planned.expanded.contents,
                )?;
            }
            write_plan(&mut out, &planned.name, &planned.plan, json)?;
        }
    }
    Ok(())
}

fn list_hosts(
    mut out: impl Write,
    repo: &Repository,
    name: Option<&str>,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    let hosts = if let Some(name) = name {
        vec![repo.select_host(HostSelector::Named(name))?]
    } else {
        repo.manifest()
            .hosts
            .iter()
            .map(|(name, entry)| (name.as_str(), entry))
            .collect()
    };
    if json {
        let rows: Vec<_> = hosts
            .iter()
            .map(|(name, entry)| {
                json!({ "name": name, "entry": entry.entry, "target": entry.target })
            })
            .collect();
        serde_json::to_writer_pretty(&mut out, &rows)?;
        writeln!(out)?;
    } else {
        for (name, entry) in hosts {
            let path = entry.entry.display();
            match &entry.target {
                Target::Local => writeln!(out, "{name}\t{path}\tlocal")?,
                Target::Ssh(alias) => writeln!(out, "{name}\t{path}\tssh {alias}")?,
            }
        }
    }
    Ok(())
}
