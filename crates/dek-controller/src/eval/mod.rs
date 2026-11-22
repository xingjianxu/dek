//! Controller-side language evaluation.

mod ruby;

pub use ruby::{EvalOutput, RubyError, RubySession, TargetAnswer, TargetProvider};
