//! Attach a target-owned helper to a separate SSH terminal.

use std::{
    ffi::OsString,
    fs::{self, File, Permissions},
    io::{self, BufReader, IsTerminal, Read},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use dek_core::target_protocol::{read_frame, valid_terminal_token, write_frame};
use rustix::{net::sockopt::socket_peercred, process::geteuid};
use serde::{Deserialize, Serialize};

use crate::{backend::BackendError, state::StateStore};

const ATTACH_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const TERMINAL_REQUIRED: &str =
    "此动作需要交互终端，请使用 dek apply --interactive 续办原执行";

/// A target backend chooses the helper and its arguments from an approved action.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalCommand {
    program: OsString,
    arguments: Vec<OsString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    directory: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    environment: Option<Vec<(OsString, OsString)>>,
}

impl TerminalCommand {
    pub fn new(
        program: impl Into<OsString>,
        arguments: impl IntoIterator<Item = impl Into<OsString>>,
    ) -> Self {
        Self {
            program: program.into(),
            arguments: arguments.into_iter().map(Into::into).collect(),
            directory: None,
            environment: None,
        }
    }

    pub fn in_environment(
        mut self,
        directory: PathBuf,
        variables: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> Self {
        self.directory = Some(directory);
        self.environment = Some(variables.into_iter().collect());
        self
    }

    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.arguments);
        if let Some(directory) = &self.directory {
            command.current_dir(directory);
        }
        if let Some(environment) = &self.environment {
            command.env_clear().envs(environment.iter().cloned());
        }
        command
    }

    fn run(&self) -> Result<(), String> {
        let mut command = self.command();
        if self.environment.is_some()
            && let Some(term) = std::env::var_os("TERM")
        {
            command.env("TERM", term);
        }
        let status = command
            .status()
            .map_err(|error| format!("无法启动交互程序 {:?}：{error}", self.program))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("交互程序 {:?} 结束：{status}", self.program))
        }
    }
}

pub(crate) fn available() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal() && io::stderr().is_terminal()
}

pub(crate) fn run_locally(command: &TerminalCommand) -> Result<(), BackendError> {
    command.run().map_err(BackendError)
}

pub(crate) type TerminalRunner<'a> =
    dyn Fn(&TerminalCommand) -> Result<(), BackendError> + Sync + 'a;

pub(crate) fn run_in_terminal(
    root: &Path,
    command: &TerminalCommand,
    require_attachment: impl FnOnce(String) -> io::Result<()>,
) -> Result<(), BackendError> {
    let result = start_attachment(root, command, require_attachment)
        .map_err(|error| BackendError(format!("交互终端：{error}")))?;
    result.map_err(BackendError)
}

fn start_attachment(
    root: &Path,
    command: &TerminalCommand,
    require_attachment: impl FnOnce(String) -> io::Result<()>,
) -> io::Result<Result<(), String>> {
    let mut nonce = [0; 16];
    File::open("/dev/urandom")?.read_exact(&mut nonce)?;
    let token = format!("{:032x}", u128::from_ne_bytes(nonce));
    let path = attachment_path(root, &token)?;
    let listener = UnixListener::bind(&path)?;
    let endpoint = AttachmentPath(path);
    fs::set_permissions(&endpoint.0, Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    require_attachment(token)?;
    let mut peer = accept(&listener)?;
    require_owner(&peer)?;
    // Close and unlink the one-use listener before running the helper.
    drop(listener);
    drop(endpoint);
    peer.set_nonblocking(false)?;
    write_frame(&mut peer, command)?;
    read_frame(&mut BufReader::new(peer))?
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "交互终端在返回结果前断开"))
}

/// Run the pending helper in this SSH session's controlling terminal.
pub fn attach(root: &Path, token: &str) -> Result<(), Box<dyn std::error::Error>> {
    let path = attachment_path(root, token)?;
    if !available() {
        return Err("此入口需要独立的 SSH 终端".into());
    }
    let _store = StateStore::open(root)?;
    let mut peer = UnixStream::connect(path)?;
    require_owner(&peer)?;
    let command: TerminalCommand = read_frame(&mut BufReader::new(&mut peer))?
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "没有待办的交互命令"))?;
    write_frame(&mut peer, &command.run())?;
    Ok(())
}

fn attachment_path(root: &Path, token: &str) -> io::Result<PathBuf> {
    if !valid_terminal_token(token) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "无效的终端附件标识",
        ));
    }
    Ok(root.join(format!("tty-{token}.sock")))
}

struct AttachmentPath(PathBuf);

impl Drop for AttachmentPath {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn accept(listener: &UnixListener) -> io::Result<UnixStream> {
    let deadline = Instant::now() + ATTACH_TIMEOUT;
    loop {
        match listener.accept() {
            Ok((peer, _)) => return Ok(peer),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "等待独立 SSH 终端超时",
                    ));
                }
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

fn require_owner(peer: &UnixStream) -> io::Result<()> {
    if socket_peercred(peer)?.uid == geteuid() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "交互终端与执行账户不一致",
        ))
    }
}
