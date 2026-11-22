//! Serve target requests using the same registry as local callers.

use std::io::{self, BufRead, Write};

use dek_core::target_protocol::{
    ClientFrame, ServerFrame, TargetEvent, TargetRequest, TargetResult, read_frame, write_frame,
};

use crate::backend::BackendRegistry;

#[cfg(target_os = "linux")]
mod execution;
#[cfg(target_os = "linux")]
pub use execution::dispatch_execution;

pub fn dispatch(registry: &BackendRegistry<'_>, operation: TargetRequest) -> TargetResult {
    match operation {
        TargetRequest::QueryInput { kind, key } => {
            match crate::inputs::observe_remote(registry, kind, &key) {
                Ok((value, observation)) => TargetResult::Input { value, observation },
                Err(error) => TargetResult::Error {
                    message: error.to_string(),
                },
            }
        }
        TargetRequest::NormalizePath { path } => match registry.normalize_path(&path) {
            Ok(normalized) => TargetResult::NormalizedPath {
                original: path,
                normalized,
            },
            Err(error) => TargetResult::Error {
                message: error.to_string(),
            },
        },
        _ => TargetResult::Error {
            message: "当前目标端不支持此请求".into(),
        },
    }
}

pub fn serve(
    registry: &BackendRegistry<'_>,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> io::Result<()> {
    serve_with(registry, input, output, |operation| {
        dispatch(registry, operation)
    })
}

pub fn serve_with(
    registry: &BackendRegistry<'_>,
    input: &mut impl BufRead,
    output: &mut impl Write,
    mut dispatch: impl FnMut(TargetRequest) -> TargetResult,
) -> io::Result<()> {
    serve_requests(registry, input, output, |_, operation, _| {
        dispatch(operation)
    })
}

pub type EventSender<'a> = dyn FnMut(TargetEvent) -> io::Result<()> + Send + 'a;

pub fn serve_events<W: Write + Send>(
    registry: &BackendRegistry<'_>,
    input: &mut impl BufRead,
    output: &mut W,
    mut dispatch: impl FnMut(TargetRequest, &mut EventSender<'_>) -> TargetResult,
) -> io::Result<()> {
    serve_requests(registry, input, output, |id, operation, output| {
        dispatch(operation, &mut |event| {
            write_frame(output, &ServerFrame::Event { id, event })
        })
    })
}

fn serve_requests<W: Write>(
    registry: &BackendRegistry<'_>,
    input: &mut impl BufRead,
    output: &mut W,
    mut dispatch: impl FnMut(u64, TargetRequest, &mut W) -> TargetResult,
) -> io::Result<()> {
    let Some(ClientFrame::Hello { versions }) = read_frame(input)? else {
        return Err(invalid("expected a target protocol hello"));
    };
    let capabilities = registry.capabilities();
    if !versions.contains(&capabilities.protocol_version) {
        return write_frame(
            output,
            &ServerFrame::Incompatible {
                versions: vec![capabilities.protocol_version],
            },
        );
    }
    write_frame(output, &ServerFrame::Hello { capabilities })?;

    let mut last_id = 0;
    while let Some(frame) = read_frame(input)? {
        let ClientFrame::Request { id, operation } = frame else {
            return Err(invalid("expected a target protocol request"));
        };
        if id <= last_id {
            return Err(invalid("target protocol request IDs must increase"));
        }
        last_id = id;
        let result = dispatch(id, operation, output);
        write_frame(output, &ServerFrame::Response { id, result })?;
    }
    Ok(())
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
