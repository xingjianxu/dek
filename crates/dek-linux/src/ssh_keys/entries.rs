use std::{
    collections::BTreeMap,
    io::Write,
    process::{Command, Stdio},
};

use base64::{Engine, engine::general_purpose::STANDARD_PAD_INDIFFERENT};
use dek_core::{AuthorizedKeysResource, ContentDigest};
use dek_target::backend::BackendError;

use crate::accounts::tools::tool;

const BEGIN: &[u8] = b"# BEGIN dek managed SSH authorized keys";
const END: &[u8] = b"# END dek managed SSH authorized keys";

pub(super) struct Document<'a> {
    pub(super) before: &'a [u8],
    pub(super) managed: &'a [u8],
    pub(super) after: &'a [u8],
}

impl<'a> Document<'a> {
    pub(super) fn read(bytes: &'a [u8]) -> Result<Self, BackendError> {
        let mut begin = None;
        let mut end = None;
        let mut offset = 0;
        for line in bytes.split_inclusive(|&byte| byte == b'\n') {
            let marker = line.strip_suffix(b"\n").unwrap_or(line);
            let marker = marker.strip_suffix(b"\r").unwrap_or(marker);
            match (marker, begin.is_some(), end.is_some()) {
                (BEGIN, false, _) => begin = Some((offset, offset + line.len())),
                (END, true, false) => end = Some((offset, offset + line.len())),
                (BEGIN | END, _, _) => return Err(markers()),
                _ => {}
            }
            offset += line.len();
        }
        match (begin, end) {
            (None, None) => Ok(Self {
                before: b"",
                managed: b"",
                after: bytes,
            }),
            (Some((start, body)), Some((end, tail))) => Ok(Self {
                before: &bytes[..start],
                managed: &bytes[body..end],
                after: &bytes[tail..],
            }),
            _ => Err(markers()),
        }
    }

    pub(super) fn render(&self, managed: &[u8]) -> Vec<u8> {
        if managed.is_empty() {
            return [self.before, self.after].concat();
        }
        [self.before, BEGIN, b"\n", managed, END, b"\n", self.after].concat()
    }

    pub(super) fn check_manual(&self, managed: &ManagedKeys) -> Result<(), BackendError> {
        for line in self
            .before
            .split(|&byte| byte == b'\n')
            .chain(self.after.split(|&byte| byte == b'\n'))
        {
            if let Some(key) = Key::parse(line)
                && managed.0.contains_key(&key.identity)
            {
                return Err(BackendError(
                    "已审阅 SSH 公钥与手工授权条目重复；请先处理手工条目".into(),
                ));
            }
        }
        Ok(())
    }
}

fn markers() -> BackendError {
    BackendError("authorized_keys 的 dek 管理标记未配对或重复".into())
}

struct Key<'a> {
    identity: Vec<u8>,
    options: &'a [u8],
    kind: &'a [u8],
    encoded: &'a [u8],
}

impl<'a> Key<'a> {
    fn parse(line: &'a [u8]) -> Option<Self> {
        let line = trim(line);
        if line.starts_with(b"#") {
            return None;
        }
        let (first, rest) = split_field(line)?;
        let (second, remainder) = split_field(rest)?;
        if let Some(identity) = identity(first, second) {
            return Some(Self {
                identity,
                options: b"",
                kind: first,
                encoded: second,
            });
        }
        let (third, _) = split_field(remainder)?;
        Some(Self {
            identity: identity(second, third)?,
            options: first,
            kind: second,
            encoded: third,
        })
    }

    fn verify_format(&self) -> Result<(), BackendError> {
        let keygen = tool("ssh-keygen")
            .map_err(|_| BackendError("目标机需要 OpenSSH 的 ssh-keygen 验证公钥".into()))?;
        let mut child = Command::new(keygen)
            .args(["-l", "-f", "/dev/stdin"])
            .env_clear()
            .env("LC_ALL", "C")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(keygen_error)?;
        let mut input = child.stdin.take().expect("piped ssh-keygen input");
        let sent = input.write_all(&[self.kind, b" ", self.encoded, b"\n"].concat());
        drop(input);
        let status = child.wait().map_err(keygen_error)?;
        sent.map_err(keygen_error)?;
        if !status.success() {
            return Err(BackendError("OpenSSH 无法读取已审阅的 SSH 公钥".into()));
        }
        Ok(())
    }
}

fn keygen_error(error: std::io::Error) -> BackendError {
    BackendError(format!("使用 ssh-keygen 验证公钥失败：{error}"))
}

fn identity(kind: &[u8], encoded: &[u8]) -> Option<Vec<u8>> {
    let data = STANDARD_PAD_INDIFFERENT.decode(encoded).ok()?;
    let length = u32::from_be_bytes(data.get(..4)?.try_into().ok()?) as usize;
    if length != kind.len() || data.get(4..)?.strip_prefix(kind)?.is_empty() {
        return None;
    }
    Some(data)
}

fn split_field(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let mut quoted = false;
    let mut bytes = line.iter().enumerate().peekable();
    while let Some((index, &byte)) = bytes.next() {
        match byte {
            b'\\' if bytes.peek().is_some_and(|(_, next)| **next == b'"') => {
                bytes.next();
            }
            b'"' => quoted = !quoted,
            b' ' | b'\t' if !quoted => return Some((&line[..index], trim(&line[index..]))),
            _ => {}
        }
    }
    (!quoted && !line.is_empty()).then_some((line, b""))
}

fn trim(bytes: &[u8]) -> &[u8] {
    bytes.trim_ascii()
}

struct ManagedKey {
    options: Vec<u8>,
    line: Vec<u8>,
}

pub(super) struct ManagedKeys(BTreeMap<Vec<u8>, ManagedKey>);

impl ManagedKeys {
    pub(super) fn read<'a>(
        declaration: &AuthorizedKeysResource,
        content: impl Fn(&ContentDigest) -> Result<&'a [u8], BackendError>,
    ) -> Result<Self, BackendError> {
        let mut managed = Self(BTreeMap::new());
        for key in &declaration.keys {
            managed.insert(key.as_bytes())?;
        }
        for digest in &declaration.key_files {
            for line in content(digest)?.split(|&byte| byte == b'\n') {
                let line = trim(line);
                if !line.is_empty() && !line.starts_with(b"#") {
                    managed.insert(line)?;
                }
            }
        }
        Ok(managed)
    }

    fn insert(&mut self, line: &[u8]) -> Result<(), BackendError> {
        if line.len() > 8192
            || line
                .iter()
                .any(|byte| matches!(byte, b'\n' | b'\r' | b'\0'))
        {
            return Err(BackendError(
                "每项 SSH 授权公钥须为不超过 8192 字节的单行条目".into(),
            ));
        }
        let key = Key::parse(line).ok_or_else(|| BackendError("无效的 SSH 授权公钥".into()))?;
        if let Some(entry) = self.0.get(&key.identity) {
            if entry.options != key.options {
                return Err(BackendError("同一 SSH 公钥的受管授权选项不一致".into()));
            }
            return Ok(());
        }
        key.verify_format()?;
        self.0.insert(
            key.identity,
            ManagedKey {
                options: key.options.to_vec(),
                line: trim(line).to_vec(),
            },
        );
        Ok(())
    }

    pub(super) fn body(&self) -> Vec<u8> {
        let mut body = Vec::new();
        for entry in self.0.values() {
            body.extend(&entry.line);
            body.push(b'\n');
        }
        body
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_managed_block_is_replaced() {
        let manual_before = &b"# comment\xff\r\n"[..];
        let manual_after = &b"# final line without a newline"[..];
        let original = [manual_before, BEGIN, b"\nold\n", END, b"\n", manual_after].concat();
        let document = Document::read(&original).unwrap();
        assert_eq!(document.managed, b"old\n");
        assert_eq!(
            document.render(b"new\n"),
            [manual_before, BEGIN, b"\nnew\n", END, b"\n", manual_after].concat()
        );
        assert_eq!(document.render(b""), [manual_before, manual_after].concat());
    }

    #[test]
    fn rejects_incomplete_nested_and_multiple_blocks() {
        assert!(Document::read(BEGIN).is_err());
        assert!(Document::read(END).is_err());
        assert!(Document::read(&[BEGIN, b"\n", BEGIN, b"\n", END, b"\n", END].concat()).is_err());
        assert!(Document::read(&[BEGIN, b"\n", END, b"\n", BEGIN, b"\n", END].concat()).is_err());
    }
}
