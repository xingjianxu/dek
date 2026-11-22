use std::{env, io, process::ExitCode};

use dek_cli::command::run_local;

fn main() -> ExitCode {
    match run_local(env::args_os(), io::stdout().lock()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("dek: {err}");
            ExitCode::FAILURE
        }
    }
}
