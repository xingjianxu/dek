//! 共享领域模型、协议与来源诊断。

mod config;
mod origin;

pub use config::{
    EffectiveOption, HostConfig, InputKind, InputObservation, InvalidOptionPath, Observation,
    OptionAssignment, OptionDefinition, OptionPath, Priority, ProfileRef, UnitSpec,
};
pub use origin::{InvalidJsonPointer, JsonPointer, Origin};
