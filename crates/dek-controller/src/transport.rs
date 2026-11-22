//! Launch the fixed target command through system OpenSSH.

mod inputs;
mod plan;

pub use inputs::{RemoteTarget, TargetInputSnapshot};

use std::{
    fs::OpenOptions,
    io::{self, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio},
};

use dek_core::{
    TargetCapabilities,
    target_protocol::{
        ClientFrame, ServerFrame, TARGET_PROTOCOL_VERSION, TargetEvent, TargetRequest,
        TargetResult, read_frame, write_frame,
    },
};
use thiserror::Error;

const REMOTE_COMMAND: &str = "dek __target-stdio";
const TERMINAL_COMMAND: &str = "dek __target-terminal";
const HANDSHAKE: &str = "握手";
const REQUEST: &str = "请求";

#[derive(Debug, Error)]
#[error("SSH 别名 {alias:?} 的{phase}失败：{kind}")]
pub struct SshError {
    pub alias: String,
    phase: &'static str,
    #[source]
    pub kind: SshErrorKind,
}

#[derive(Debug, Error)]
pub enum SshErrorKind {
    #[error("无法启动 OpenSSH：{0}")]
    Spawn(#[source] io::Error),
    #[error("OpenSSH 连接失败（退出码 255，详情见 SSH 诊断）")]
    Connection,
    #[error("目标机找不到 dek 可执行文件（退出码 127）")]
    RemoteBinaryMissing,
    #[error("目标端连接结束：{0}")]
    Exited(ExitStatus),
    #[error("目标协议数据无效：{0}")]
    Protocol(#[source] io::Error),
    #[error("无法与目标端交换数据：{0}")]
    Io(#[source] io::Error),
    #[error("目标协议版本不兼容，本机支持 {TARGET_PROTOCOL_VERSION}，目标支持 {remote:?}")]
    Version { remote: Vec<u32> },
    #[error("目标标识不匹配，预期 {expected:?}，收到 {actual:?}")]
    Identity { expected: String, actual: String },
    #[error("目标端拒绝请求：{0}")]
    Target(String),
}

/// An SSH process whose host key, protocol version and target ID have been checked.
pub struct SshSession {
    alias: String,
    capabilities: TargetCapabilities,
    process: Option<SshProcess>,
    last_id: u64,
}

impl SshSession {
    pub fn connect(alias: &str, expected_id: &str) -> Result<Self, SshError> {
        let error = |kind| ssh_error(alias, HANDSHAKE, kind);
        let mut process = SshProcess::connect(alias).map_err(error)?;
        process
            .write(&ClientFrame::Hello {
                versions: vec![TARGET_PROTOCOL_VERSION],
            })
            .map_err(error)?;
        let capabilities = match process.read().map_err(error)? {
            ServerFrame::Hello { capabilities }
                if capabilities.protocol_version == TARGET_PROTOCOL_VERSION =>
            {
                capabilities
            }
            ServerFrame::Hello { capabilities } => {
                return Err(error(SshErrorKind::Version {
                    remote: vec![capabilities.protocol_version],
                }));
            }
            ServerFrame::Incompatible { versions } => {
                return Err(error(SshErrorKind::Version { remote: versions }));
            }
            ServerFrame::Response { .. } | ServerFrame::Event { .. } => {
                return Err(error(invalid("expected a target protocol hello")));
            }
        };
        if capabilities.target.id != expected_id {
            return Err(error(SshErrorKind::Identity {
                expected: expected_id.into(),
                actual: capabilities.target.id,
            }));
        }

        Ok(Self {
            alias: alias.into(),
            capabilities,
            process: Some(process),
            last_id: 0,
        })
    }

    pub fn capabilities(&self) -> &TargetCapabilities {
        &self.capabilities
    }

    pub fn request(&mut self, operation: TargetRequest) -> Result<TargetResult, SshError> {
        self.request_with_events(operation, |_| {
            Err(invalid("unexpected target protocol event"))
        })
    }

    pub fn request_with_events(
        &mut self,
        operation: TargetRequest,
        on_event: impl FnMut(TargetEvent) -> Result<(), SshErrorKind>,
    ) -> Result<TargetResult, SshError> {
        let result = self.exchange(operation, on_event);
        if result.is_err() {
            self.process.take();
        }
        result.map_err(|kind| ssh_error(&self.alias, REQUEST, kind))
    }

    fn exchange(
        &mut self,
        operation: TargetRequest,
        mut on_event: impl FnMut(TargetEvent) -> Result<(), SshErrorKind>,
    ) -> Result<TargetResult, SshErrorKind> {
        let process = self
            .process
            .as_mut()
            .ok_or_else(|| invalid("SSH session is closed"))?;
        let id = self
            .last_id
            .checked_add(1)
            .ok_or_else(|| invalid("target protocol request ID overflow"))?;
        self.last_id = id;
        process.write(&ClientFrame::Request { id, operation })?;
        loop {
            match process.read()? {
                ServerFrame::Response { id: reply, result } if id == reply => return Ok(result),
                ServerFrame::Event { id: reply, event } if id == reply => on_event(event)?,
                ServerFrame::Response { .. } | ServerFrame::Event { .. } => {
                    return Err(invalid("target protocol response ID did not match"));
                }
                _ => return Err(invalid("expected a target protocol response")),
            }
        }
    }

    pub fn finish(mut self) -> Result<(), SshError> {
        let Some(mut process) = self.process.take() else {
            return Ok(());
        };
        process
            .finish()
            .map_err(|kind| ssh_error(&self.alias, "会话收尾", kind))
    }
}

/// Attach the user's controlling terminal directly to a previously bound target helper.
pub fn attach_terminal(alias: &str, token: &str) -> Result<(), SshErrorKind> {
    if !dek_core::target_protocol::valid_terminal_token(token) {
        return Err(invalid("invalid target terminal token"));
    }
    let tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .map_err(|error| SshErrorKind::Target(format!("无法打开管理机终端：{error}")))?;
    let output = tty.try_clone().map_err(SshErrorKind::Io)?;
    let diagnostic = tty.try_clone().map_err(SshErrorKind::Io)?;
    let status = ssh_command("-tt", alias)
        .arg(format!("{TERMINAL_COMMAND} {token}"))
        .stdin(Stdio::from(tty))
        .stdout(Stdio::from(output))
        .stderr(Stdio::from(diagnostic))
        .status()
        .map_err(SshErrorKind::Spawn)?;
    if status.success() {
        Ok(())
    } else {
        Err(exit_error(status))
    }
}

fn ssh_command(tty: &str, alias: &str) -> Command {
    let mut command = Command::new("ssh");
    command.args([
        tty,
        "-S",
        "none",
        "-o",
        "BatchMode=yes",
        "-o",
        "StrictHostKeyChecking=yes",
        "--",
        alias,
    ]);
    command
}

struct SshProcess {
    child: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
}

impl SshProcess {
    fn connect(alias: &str) -> Result<Self, SshErrorKind> {
        let mut child = ssh_command("-T", alias)
            .arg(REMOTE_COMMAND)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(SshErrorKind::Spawn)?;
        let input = child.stdin.take().expect("SSH stdin was piped");
        let output = child.stdout.take().expect("SSH stdout was piped");
        Ok(Self {
            child,
            input: Some(input),
            output: BufReader::new(output),
        })
    }

    fn write(&mut self, frame: &ClientFrame) -> Result<(), SshErrorKind> {
        let input = self.input.as_mut().expect("SSH session input is open");
        write_frame(input, frame).map_err(|error| {
            if error.kind() == io::ErrorKind::BrokenPipe {
                return match self.child.wait() {
                    Ok(status) if !status.success() => exit_error(status),
                    _ => SshErrorKind::Io(error),
                };
            }
            SshErrorKind::Io(error)
        })
    }

    fn read(&mut self) -> Result<ServerFrame, SshErrorKind> {
        if let Some(frame) = self.read_optional()? {
            return Ok(frame);
        }
        self.input.take();
        Err(exit_error(self.child.wait().map_err(SshErrorKind::Io)?))
    }

    fn read_optional(&mut self) -> Result<Option<ServerFrame>, SshErrorKind> {
        read_frame(&mut self.output).map_err(|error| {
            if error.kind() == io::ErrorKind::InvalidData {
                SshErrorKind::Protocol(error)
            } else {
                SshErrorKind::Io(error)
            }
        })
    }

    fn finish(&mut self) -> Result<(), SshErrorKind> {
        self.input.take();
        if self.read_optional()?.is_some() {
            return Err(invalid(
                "unexpected target protocol frame after session close",
            ));
        }
        let status = self.child.wait().map_err(SshErrorKind::Io)?;
        if status.success() {
            Ok(())
        } else {
            Err(exit_error(status))
        }
    }
}

impl Drop for SshProcess {
    fn drop(&mut self) {
        self.input.take();
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn ssh_error(alias: &str, phase: &'static str, kind: SshErrorKind) -> SshError {
    SshError {
        alias: alias.into(),
        phase,
        kind,
    }
}

fn invalid(message: &'static str) -> SshErrorKind {
    SshErrorKind::Protocol(io::Error::new(io::ErrorKind::InvalidData, message))
}

fn exit_error(status: ExitStatus) -> SshErrorKind {
    match status.code() {
        Some(127) => SshErrorKind::RemoteBinaryMissing,
        Some(255) => SshErrorKind::Connection,
        _ => SshErrorKind::Exited(status),
    }
}
