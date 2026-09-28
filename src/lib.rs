//! Asset-backed hierarchical statecharts for Bevy.
//!
//! State tree definitions are immutable assets shared by any number of
//! entity-local instances. Applications connect Rust event types to authored
//! transition names with [`StateTreeAppExt`].
//!
//! # Event ordering
//!
//! Each [`StateTreeInstance`] has an independent ordered external event queue.
//! [`EntityStateTreeEventWriter`] writes to one entity's queue, while
//! [`BroadcastStateTreeEventWriter`] inserts an event at the corresponding
//! position in every initialized instance's queue. A complete macrostep runs
//! for one event before that instance receives its next event. Separate
//! instances are processed in parallel and have no shared event order.
//!
//! Concurrent writes to the same instance have a valid but unspecified order.
//! Order the producer systems explicitly when the result must be deterministic.
//! Put producers in [`StateTreeSet::QueueEvents`] when their events must be
//! delivered by [`StateTreeSet::ProcessEvents`] in the same schedule run.
//!
//! Event recipients are determined when processing begins. An instance that
//! initializes during the same schedule run receives already queued entity and
//! broadcast events. An instance still pending at that point receives neither;
//! its queued entity events are discarded rather than replayed after a later
//! initialization.

mod definition;
mod instance;
mod plugin;

pub use definition::{BuildError, StateId, StateTreeBuilder, StateTreeDefinition};
pub use instance::{InstanceStatus, RuntimeFault, StateTreeInstance};
pub use plugin::{
    BroadcastStateTreeEventWriter, EntityStateTreeEventWriter, StateTreeAppExt, StateTreePlugin,
    StateTreeSet, StateTreeSettings,
};

/// Common state-tree imports.
pub mod prelude {
    pub use crate::{
        BroadcastStateTreeEventWriter, BuildError, EntityStateTreeEventWriter, InstanceStatus,
        RuntimeFault, StateId, StateTreeAppExt, StateTreeBuilder, StateTreeDefinition,
        StateTreeInstance, StateTreePlugin, StateTreeSet, StateTreeSettings,
    };
}
