//! Asset-backed hierarchical statecharts for Bevy.
//!
//! State tree definitions are immutable assets shared by any number of
//! entity-local instances. Plugins register typed external events into each
//! domain's protocol before execution begins.
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
mod config;
mod definition;
mod effect;
mod event;
mod instance;
mod luau;
mod plugin;
mod trace;

#[cfg(test)]
mod test_harness;

pub mod scxml;

pub use bevy_state_tree_macros::StateTreeEvent;
pub use config::StateTreeConfig;
pub use definition::{
    BuildError, HistoryId, StateId, StateTreeAction, StateTreeActivityDefinition,
    StateTreeArgument, StateTreeBuilder, StateTreeData, StateTreeDefinition,
    StateTreeEffectArgument, StateTreeLinkError, StateTreeLinkIssue, StateTreeTarget,
    StateTreeTransition,
};
pub use effect::{
    FromStateTreeValue, StateTreeActivityArgs, StateTreeActivityContext, StateTreeActivityError,
    StateTreeArgs, StateTreeContext, StateTreeEffectArgs, StateTreeEffectContext,
    StateTreeEffectError,
};
pub use event::{
    BroadcastStateTreeEvent, EntityStateTreeEvent, StateTreeDomain, StateTreeEvent,
    StateTreeEventDataError, StateTreeEventRef,
};
pub use instance::{
    DatamodelOperation, InstanceStatus, RuntimeFault, StateTreeDataOverrideError,
    StateTreeDataOverrides, StateTreeInstance,
};
pub use luau::LuauValue;
pub use plugin::{
    BroadcastStateTreeEventWriter, EntityStateTreeEventWriter, StateTreeLinkFailure,
    StateTreePlugin, StateTreeSet, StateTreeSettings,
};
pub(crate) use trace::{
    NoStateTreeTrace, StateTreeHistoryResolution, StateTreeTrace, StateTreeTraceRecord,
    StateTreeTraceTrigger,
};
pub use trace::{StateTreeEventLog, StateTreeTraceLog, StateTreeTracePlugin};

/// Common state-tree imports.
pub mod prelude {
    pub use crate::{
        BroadcastStateTreeEvent, BroadcastStateTreeEventWriter, BuildError, DatamodelOperation,
        EntityStateTreeEvent, EntityStateTreeEventWriter, FromStateTreeValue, HistoryId,
        InstanceStatus, LuauValue, RuntimeFault, StateId, StateTreeAction, StateTreeActivity,
        StateTreeActivityArgs, StateTreeActivityContext, StateTreeActivityDefinition,
        StateTreeActivityError, StateTreeActivityLifecycle, StateTreeActivityOutcomeWriter,
        StateTreeActivityToken, StateTreeArgs, StateTreeArgument, StateTreeBuilder,
        StateTreeConfig, StateTreeContext, StateTreeData, StateTreeDataOverrideError,
        StateTreeDataOverrides, StateTreeDefinition, StateTreeDomain, StateTreeEffectArgs,
        StateTreeEffectArgument, StateTreeEffectContext, StateTreeEffectError, StateTreeEvent,
        StateTreeEventDataError, StateTreeEventLog, StateTreeEventRef, StateTreeInstance,
        StateTreeLinkError, StateTreeLinkFailure, StateTreeLinkIssue, StateTreePlugin,
        StateTreeSet, StateTreeSettings, StateTreeTarget, StateTreeTraceLog, StateTreeTracePlugin,
        StateTreeTransition,
    };
}
pub use activity::{
    StateTreeActivity, StateTreeActivityLifecycle, StateTreeActivityOutcomeWriter,
    StateTreeActivityToken,
};
