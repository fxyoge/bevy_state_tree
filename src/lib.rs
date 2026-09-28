//! Asset-backed hierarchical statecharts for Bevy.
//!
//! State tree definitions are immutable assets shared by any number of
//! entity-local instances. Applications connect ordinary Bevy messages to
//! authored transition names with [`StateTreeAppExt`].

mod definition;
mod instance;
mod plugin;

pub use definition::{BuildError, StateId, StateTreeBuilder, StateTreeDefinition};
pub use instance::{InstanceStatus, RuntimeFault, StateTreeInstance};
pub use plugin::{StateTreeAppExt, StateTreePlugin, StateTreeSet, StateTreeSettings};

/// Common state-tree imports.
pub mod prelude {
    pub use crate::{
        BuildError, InstanceStatus, RuntimeFault, StateId, StateTreeAppExt, StateTreeBuilder,
        StateTreeDefinition, StateTreeInstance, StateTreePlugin, StateTreeSet, StateTreeSettings,
    };
}
