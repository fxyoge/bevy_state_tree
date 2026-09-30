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

mod activity;
mod definition;
mod effect;
mod event;
mod instance;
mod luau;
mod plugin;
mod trace;

pub mod scxml;

pub use bevy_state_tree_macros::StateTreeEvents;
pub use definition::{
    BuildError, StateId, StateTreeAction, StateTreeActivityDefinition, StateTreeArgument,
    StateTreeBuilder, StateTreeData, StateTreeDefinition, StateTreeEffectArgument,
    StateTreeLinkError, StateTreeLinkIssue, StateTreeTransition,
};
pub use effect::{
    FromStateTreeValue, StateTreeActivityArgs, StateTreeActivityContext, StateTreeActivityError,
    StateTreeAppExt, StateTreeArgs, StateTreeContext, StateTreeEffectArgs, StateTreeEffectContext,
    StateTreeEffectError, StateTreeEffectRequest, StateTreeEffectSink,
};
pub use event::{
    BroadcastStateTreeEvent, EntityStateTreeEvent, StateTreeDomain, StateTreeEventDataError,
    StateTreeEventPayload, StateTreeEvents,
};
pub use instance::{
    DatamodelOperation, InstanceStatus, RuntimeFault, StateTreeExecution, StateTreeInstance,
};
pub use luau::LuauValue;
pub use plugin::{
    BroadcastStateTreeEventWriter, EntityStateTreeEventWriter, StateTreeLinkFailure,
    StateTreePlugin, StateTreeSet, StateTreeSettings,
};
pub use trace::{
    NoStateTreeTrace, StateTreeEventLog, StateTreeTrace, StateTreeTraceRecord,
    StateTreeTraceTrigger,
};

/// Common state-tree imports.
pub mod prelude {
    pub use crate::{
        BroadcastStateTreeEvent, BroadcastStateTreeEventWriter, BuildError, DatamodelOperation,
        EntityStateTreeEvent, EntityStateTreeEventWriter, FromStateTreeValue, InstanceStatus,
        LuauValue, RuntimeFault, StateId, StateTreeAction, StateTreeActivity,
        StateTreeActivityArgs, StateTreeActivityContext, StateTreeActivityDefinition,
        StateTreeActivityError, StateTreeActivityLifecycle, StateTreeActivityOutcomeWriter,
        StateTreeActivityRequest, StateTreeActivitySink, StateTreeActivityToken, StateTreeAppExt,
        StateTreeArgs, StateTreeArgument, StateTreeBuilder, StateTreeContext, StateTreeData,
        StateTreeDefinition, StateTreeDomain, StateTreeEffectArgs, StateTreeEffectArgument,
        StateTreeEffectContext, StateTreeEffectError, StateTreeEffectRequest, StateTreeEffectSink,
        StateTreeEventDataError, StateTreeEventLog, StateTreeEventPayload, StateTreeEvents,
        StateTreeExecution, StateTreeHeadlessActivityToken, StateTreeInstance, StateTreeLinkError,
        StateTreeLinkFailure, StateTreeLinkIssue, StateTreePlugin, StateTreeSet, StateTreeSettings,
        StateTreeTrace, StateTreeTraceRecord, StateTreeTraceTrigger, StateTreeTransition,
    };
}
pub use activity::{
    StateTreeActivity, StateTreeActivityLifecycle, StateTreeActivityOutcomeWriter,
    StateTreeActivityRequest, StateTreeActivitySink, StateTreeActivityToken,
    StateTreeHeadlessActivityToken,
};
