use std::{env, process::ExitCode};

fn main() -> ExitCode {
    let mut args = env::args_os().skip(1);
    let first = args.next();
    if args.next().is_none() {
        match first.as_deref().and_then(|arg| arg.to_str()) {
            Some("--version" | "-V") => {
                println!("dek {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            Some("--help" | "-h") => {
                println!("dek · 声明式主机配置管理器\n\n用法：dek --help | --version");
                return ExitCode::SUCCESS;
            }
            _ => {}
        }
    }
    eprintln!("当前项目骨架支持 dek --help 和 dek --version。");
    ExitCode::FAILURE
}
