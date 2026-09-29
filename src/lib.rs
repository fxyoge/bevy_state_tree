//! Asset-backed hierarchical statecharts for Bevy.
//!
//! State tree definitions are immutable assets shared by any number of
//! entity-local instances. Each domain declares its closed external event
//! protocol with [`StateTreeEvents`](macro@StateTreeEvents).
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

extern crate self as bevy_state_tree;

mod definition;
mod event;
mod instance;
mod luau;
mod plugin;
mod trace;

pub mod scxml;

pub use bevy_state_tree_macros::StateTreeEvents;
pub use definition::{BuildError, StateId, StateTreeBuilder, StateTreeDefinition};
pub use event::{
    BroadcastStateTreeEvent, EntityStateTreeEvent, StateTreeDomain, StateTreeEventPayload,
    StateTreeEvents,
};
pub use instance::{
    DatamodelOperation, InstanceStatus, RuntimeFault, StateTreeExecution, StateTreeInstance,
};
pub use luau::LuauValue;
pub use plugin::{
    BroadcastStateTreeEventWriter, EntityStateTreeEventWriter, StateTreePlugin, StateTreeSet,
    StateTreeSettings,
};
pub use trace::{
    NoStateTreeTrace, StateTreeEventLog, StateTreeTrace, StateTreeTraceRecord,
    StateTreeTraceTrigger,
};

/// Common state-tree imports.
pub mod prelude {
    pub use crate::{
        BroadcastStateTreeEvent, BroadcastStateTreeEventWriter, BuildError, DatamodelOperation,
        EntityStateTreeEvent, EntityStateTreeEventWriter, InstanceStatus, LuauValue, RuntimeFault,
        StateId, StateTreeBuilder, StateTreeDefinition, StateTreeDomain, StateTreeEventLog,
        StateTreeEventPayload, StateTreeEvents, StateTreeExecution, StateTreeInstance,
        StateTreePlugin, StateTreeSet, StateTreeSettings, StateTreeTrace, StateTreeTraceRecord,
        StateTreeTraceTrigger,
    };
}
