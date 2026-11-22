#![cfg(target_os = "linux")]

use std::{env, path::Path, process::Command};

#[test]
#[ignore = "requires dedicated offline Arch guests and an Arch-compatible dek binary"]
fn signed_arch_rollout_and_recovery() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/tests/arch-rollout");
    let image = env::var("DEK_TEST_ARCH_IMAGE").expect("set DEK_TEST_ARCH_IMAGE");
    let binary = env::var_os("DEK_TEST_ARCH_BINARY").expect("set DEK_TEST_ARCH_BINARY");
    let status = Command::new(script)
        .arg(image)
        .arg(env!("CARGO_BIN_EXE_dek"))
        .arg(binary)
        .status()
        .expect("start signed Arch rollout acceptance");
    assert!(status.success(), "{status}");
}
