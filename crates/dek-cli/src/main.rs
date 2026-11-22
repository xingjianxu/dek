use std::{
    env,
    error::Error,
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
};

use clap::{Parser, Subcommand, ValueEnum};
use dek_controller::repo::{EntryLanguage, HostSelector, Repository, Target, initialize};
use serde_json::json;

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
}

#[derive(Clone, Copy, ValueEnum)]
enum Language {
    Json,
    Ruby,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("dek: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn Error>> {
    let mut out = io::stdout().lock();
    match cli.command {
        Command::Init { directory, lang } => {
            if cli.host.is_some() {
                return Err("dek init 始终使用当前主机名，请省略 --host".into());
            }
            if directory.is_some() && cli.repo.is_some() {
                return Err("初始化目录只能通过位置参数或 --repo 指定一次".into());
            }
            let root = directory.or(cli.repo).unwrap_or_else(|| PathBuf::from("."));
            let hostname = hostname::get()
                .map_err(|err| format!("无法读取当前主机名：{err}"))?
                .into_string()
                .map_err(|name| format!("当前主机名不是有效的 UTF-8：{name:?}"))?;
            let language = match lang {
                Language::Json => EntryLanguage::Json,
                Language::Ruby => EntryLanguage::Ruby,
            };
            initialize(&root, &hostname, language)?;
            writeln!(out, "已初始化配置仓库 {}", root.display())?;
        }
        Command::Hosts { json } => {
            let repo = Repository::discover(&env::current_dir()?, cli.repo.as_deref())?;
            list_hosts(&mut out, &repo, cli.host.as_deref(), json)?;
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
