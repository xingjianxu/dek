//! Local CLI command adapters and shared planning entry points.

#[cfg(target_os = "linux")]
pub mod applying;
pub mod command;
pub mod evaluation;
pub mod local_target;
pub mod plan_output;
pub mod planning;

mod refresh;
#[cfg(target_os = "linux")]
mod remote_execution;
mod repository;
mod show;
#[cfg(target_os = "linux")]
mod status;
#[cfg(target_os = "linux")]
mod upgrading;
