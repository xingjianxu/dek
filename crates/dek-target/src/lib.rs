//! 目标端负责规划、执行、持久化和资源后端接口。

pub mod backend;
pub mod planner;
#[cfg(target_os = "linux")]
pub mod state;
