//! Serve target requests using the same registry as local callers.

use std::io::{self, BufRead, Write};

use dek_core::target_protocol::{
    ClientFrame, ServerFrame, TargetRequest, TargetResult, read_frame, write_frame,
};

use crate::backend::BackendRegistry;

pub fn dispatch(registry: &BackendRegistry<'_>, operation: TargetRequest) -> TargetResult {
    match operation {
        TargetRequest::NormalizePath { path } => match registry.normalize_path(&path) {
            Ok(normalized) => TargetResult::NormalizedPath {
                original: path,
                normalized,
            },
            Err(error) => TargetResult::Error {
                message: error.to_string(),
            },
        },
    }
}

pub fn serve(
    registry: &BackendRegistry<'_>,
    input: &mut impl BufRead,
    output: &mut impl Write,
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
        let result = dispatch(registry, operation);
        write_frame(output, &ServerFrame::Response { id, result })?;
    }
    Ok(())
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
