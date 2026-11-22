//! Static option schemas and source-preserving configuration merging.

mod merge;
mod schema;

pub use merge::MergeError;
pub use schema::{SchemaError, SchemaLocation, SchemaRegistry, ValueMismatch};

mod expansion;
mod resources;

pub use expansion::{
    ExpandContext, ExpansionError, ModuleDefinition, ModuleExpansion, ModuleRegistry,
};
pub use resources::{
    DeclaredResource, ExpandedResources, FileSource, ModuleOutput, ResourceDraft,
    assemble_resources,
};

mod repository_modules;
pub use repository_modules::{RepositoryModuleError, RepositoryModules};
