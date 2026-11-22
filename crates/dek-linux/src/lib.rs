//! Linux 与 Arch 资源后端。

#[cfg(target_os = "linux")]
pub mod accounts;

#[cfg(target_os = "linux")]
pub mod secrets;

#[cfg(target_os = "linux")]
pub mod filesystem;

#[cfg(target_os = "linux")]
pub mod systemd;
