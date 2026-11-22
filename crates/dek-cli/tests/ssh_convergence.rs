#![cfg(target_os = "linux")]

use std::{env, path::Path, process::Command};

#[test]
#[ignore = "requires two dedicated offline Arch guests and an Arch-compatible dek binary"]
fn real_ssh_service_convergence() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/tests/ssh-convergence");
    let image = env::var("DEK_TEST_ARCH_IMAGE").expect("set DEK_TEST_ARCH_IMAGE");
    let binary = env::var_os("DEK_TEST_ARCH_BINARY").expect("set DEK_TEST_ARCH_BINARY");
    let status = Command::new(script)
        .arg(image)
        .arg(env!("CARGO_BIN_EXE_dek"))
        .arg(binary)
        .status()
        .expect("start the SSH convergence fixture");
    assert!(status.success(), "{status}");
}
