//! Static option schemas and source-preserving configuration merging.

mod merge;
mod schema;

pub use merge::MergeError;
pub use schema::{SchemaError, SchemaLocation, SchemaRegistry, ValueMismatch};
