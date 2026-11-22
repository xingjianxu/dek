use std::{env, io, process::ExitCode};

use dek_cli::{
    command::run,
    local_target::{LocalFileBackend, local_registry},
};

fn main() -> ExitCode {
    match run(env::args_os(), io::stdout().lock(), || {
        local_registry(&LocalFileBackend)
    }) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("dek: {err}");
            ExitCode::FAILURE
        }
    }
}
