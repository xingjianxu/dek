use std::{
    error::Error,
    ffi::OsString,
    io::{self, Write},
    path::PathBuf,
};

#[cfg(not(target_os = "linux"))]
use crate::plan_output::write_plan;
use crate::{
    evaluation::normal_cache, local_target::LocalEvaluationTarget, planning::plan_host_with_cache,
};
use clap::{Parser, Subcommand, ValueEnum};
use dek_controller::{
    repo::{EntryLanguage, HostSelector, Repository, Target, initialize},
    transport::SshSession,
};
use dek_core::{OptionPath, TargetCapabilities};
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
    /// 核对目标身份并显示协议版本与资源能力。
    TargetInfo {
        /// 输出 JSON。
        #[arg(long)]
        json: bool,
    },
    #[command(name = "__target-stdio", hide = true)]
    TargetStdio,
    /// 执行当次规划或指定的本机已审阅计划。
    #[cfg(target_os = "linux")]
    Apply {
        /// 已审阅的计划 ID。
        #[arg(long, value_name = "计划ID")]
        plan: Option<String>,
        /// 恢复指定的执行记录。
        #[arg(long, value_name = "执行ID", conflicts_with = "plan")]
        run: Option<String>,
        /// 同一计划可重发的请求标识。
        #[arg(long, value_name = "标识", requires = "plan")]
        request_id: Option<String>,
        /// 使用指定的私有目标状态目录。默认 /var/lib/dek。
        #[arg(long, value_name = "目录")]
        state_dir: Option<PathBuf>,
    },
    /// 查看最近或指定的本机执行记录。
    #[cfg(target_os = "linux")]
    Status {
        /// 指定执行 ID。默认显示最近执行。
        #[arg(long, value_name = "执行ID")]
        run: Option<String>,
        /// 使用指定的私有目标状态目录。默认 /var/lib/dek。
        #[arg(long, value_name = "目录")]
        state_dir: Option<PathBuf>,
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
    out: impl Write,
    local_target: impl FnOnce() -> Result<BackendRegistry<'target>, Box<dyn Error>>,
) -> Result<(), Box<dyn Error>> {
    run_command(args, out, local_target, false)
}

/// Run with the installed binary's local resource backends.
pub fn run_local(
    args: impl IntoIterator<Item = impl Into<OsString> + Clone>,
    out: impl Write,
) -> Result<(), Box<dyn Error>> {
    use crate::local_target::local_registry;
    #[cfg(target_os = "linux")]
    let files = dek_linux::filesystem::FileBackend::default();
    #[cfg(not(target_os = "linux"))]
    let files = crate::local_target::LocalFileBackend;
    run_command(args, out, || local_registry(&files), true)
}

fn run_command<'target>(
    args: impl IntoIterator<Item = impl Into<OsString> + Clone>,
    mut out: impl Write,
    local_target: impl FnOnce() -> Result<BackendRegistry<'target>, Box<dyn Error>>,
    _production: bool,
) -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse_from(args);
    #[cfg(target_os = "linux")]
    let secrets = {
        use dek_target::state::DEFAULT_ROOT;
        let root = match &cli.command {
            Command::Plan {
                state_dir: Some(root),
                ..
            }
            | Command::Apply {
                state_dir: Some(root),
                ..
            } => root.clone(),
            _ => PathBuf::from(DEFAULT_ROOT),
        };
        dek_linux::secrets::SecretBackend::new(root)
    };
    #[cfg(target_os = "linux")]
    let pacman = dek_linux::arch::PacmanBackend::default();
    #[cfg(target_os = "linux")]
    let systemd = dek_linux::systemd::SystemdBackend;
    #[cfg(target_os = "linux")]
    let accounts = dek_linux::accounts::AccountBackend::default();
    #[cfg(target_os = "linux")]
    let authorized_keys = dek_linux::ssh_keys::AuthorizedKeysBackend;
    #[cfg(target_os = "linux")]
    let directories = dek_linux::filesystem::DirectoryBackend;
    #[cfg(target_os = "linux")]
    let local_target = || {
        let mut registry = local_target()?;
        if _production {
            registry.register(&secrets)?;
            if registry.target().distribution.as_deref() == Some("arch") && pacman.available() {
                registry.register(&pacman)?;
            }
            registry.register(&systemd)?;
            registry.register(&accounts)?;
            registry.register(&authorized_keys)?;
            registry.register(&directories)?;
        }
        Ok::<_, Box<dyn Error>>(registry)
    };
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
        Command::TargetInfo { json } => {
            let repo = discover_repo(cli.repo.as_deref())?;
            let selection = HostSelection::from_arg(cli.host)?;
            let (name, entry) = repo.select_host(selection.selector())?;
            let capabilities = match &entry.target {
                Target::Local => local_target()?.capabilities(),
                Target::Ssh(alias) => {
                    let expected = entry.target_id.as_deref().unwrap_or(name);
                    let session = SshSession::connect(alias, expected)?;
                    let capabilities = session.capabilities().clone();
                    session.finish()?;
                    capabilities
                }
            };
            write_target_info(&mut out, &capabilities, json)?;
        }
        Command::TargetStdio => {
            if cli.repo.is_some() || cli.host.is_some() {
                return Err("目标端入口无需 --repo 或 --host".into());
            }
            dek_target::transport::serve(&local_target()?, &mut io::stdin().lock(), &mut out)?;
        }
        #[cfg(target_os = "linux")]
        Command::Apply {
            plan,
            run,
            request_id,
            state_dir,
        } => {
            use dek_target::state::DEFAULT_ROOT;

            let repo = discover_repo(cli.repo.as_deref())?;
            let selection = HostSelection::from_arg(cli.host)?;
            let registry = local_target()?;
            let state_dir = state_dir.unwrap_or_else(|| PathBuf::from(DEFAULT_ROOT));
            crate::applying::apply_local(
                &mut out,
                &repo,
                selection.selector(),
                &registry,
                &state_dir,
                crate::applying::ApplySelection {
                    plan: plan.as_deref(),
                    run: run.as_deref(),
                    request_id: request_id.as_deref(),
                },
            )?;
        }
        #[cfg(target_os = "linux")]
        Command::Status {
            run,
            state_dir,
            json,
        } => {
            use dek_target::state::DEFAULT_ROOT;

            let repo = discover_repo(cli.repo.as_deref())?;
            let selection = HostSelection::from_arg(cli.host)?;
            let registry = local_target()?;
            let state_dir = state_dir.unwrap_or_else(|| PathBuf::from(DEFAULT_ROOT));
            crate::status::show_status(
                &mut out,
                &repo,
                selection.selector(),
                registry.target(),
                &state_dir,
                run.as_deref(),
                json,
            )?;
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
            let (plan, pending) = {
                use dek_target::state::{DEFAULT_ROOT, PlanBinding, StateStore};

                let state_dir = state_dir.unwrap_or_else(|| PathBuf::from(DEFAULT_ROOT));
                let store = StateStore::open(&state_dir).map_err(|err| {
                    format!(
                        "{err}。请使用持有目标状态目录的账户，或用 --state-dir 指定该账户的私有目录"
                    )
                })?;
                let guard = store.lock()?;
                let plan = guard.save_plan(
                    &planned.name,
                    &planned.plan,
                    &planned.expanded.resources,
                    &planned.expanded.contents,
                )?;
                let pending = guard.pending_notifications(PlanBinding {
                    repository_id: &plan.repository_id,
                    logical_host: &planned.name,
                    target: registry.target(),
                })?;
                (plan, pending)
            };
            #[cfg(target_os = "linux")]
            crate::status::write_plan_with_pending(&mut out, &planned.name, &plan, json, &pending)?;
            #[cfg(not(target_os = "linux"))]
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

fn write_target_info(
    mut out: impl Write,
    capabilities: &TargetCapabilities,
    json: bool,
) -> Result<(), Box<dyn Error>> {
    if json {
        serde_json::to_writer_pretty(&mut out, capabilities)?;
        writeln!(out)?;
        return Ok(());
    }
    let target = &capabilities.target;
    writeln!(
        out,
        "目标 {}（{} {} {}）",
        target.id,
        target.os,
        target.distribution.as_deref().unwrap_or("未知发行版"),
        target.architecture,
    )?;
    writeln!(out, "协议版本 {}", capabilities.protocol_version)?;
    for capability in &capabilities.resources {
        let attributes = capability.attributes.iter().cloned().collect::<Vec<_>>();
        writeln!(
            out,
            "{:?} v{} {}",
            capability.kind,
            capability.version,
            attributes.join(", "),
        )?;
    }
    Ok(())
}
