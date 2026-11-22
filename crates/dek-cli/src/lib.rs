//! Local CLI command adapters and shared planning entry points.

#[cfg(target_os = "linux")]
pub mod applying;
pub mod command;
pub mod evaluation;
pub mod local_target;
pub mod plan_output;
pub mod planning;

mod refresh;
mod repository;
mod show;
#[cfg(target_os = "linux")]
mod status;
