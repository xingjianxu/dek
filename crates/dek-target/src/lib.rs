//! 目标端负责规划、执行、持久化和资源后端接口。

pub mod backend;
#[cfg(target_os = "linux")]
pub mod executor;
pub mod inputs;
pub mod planner;
#[cfg(target_os = "linux")]
pub mod state;
#[cfg(target_os = "linux")]
pub mod terminal;
pub mod transport;
