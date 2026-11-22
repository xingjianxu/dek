//! Static option schemas and source-preserving configuration merging.

mod merge;
mod schema;

pub use merge::MergeError;
pub use schema::{SchemaError, SchemaLocation, SchemaRegistry, ValueMismatch};

mod builtin_apps;
pub use builtin_apps::{register_app_modules, register_app_schemas};

mod expansion;
mod resources;

pub use expansion::{
    ExpandContext, ExpansionError, ModuleDefinition, ModuleExpansion, ModuleRegistry,
};
pub use resources::{
    DeclaredResource, ExpandedResources, FileSource, ModuleOutput, ResourceDraft,
    assemble_resources,
};
