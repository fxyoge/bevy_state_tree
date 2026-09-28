use bevy_ecs::event::{EntityEvent, Event};

/// Identifies one independent state-tree runtime and its external event protocol.
pub trait StateTreeDomain: Send + Sync + 'static {
    /// Closed set of external events accepted by this domain.
    type Event: StateTreeEvents;
}

/// A domain's closed set of external state-tree events.
///
/// Implementations are normally generated with
/// [`StateTreeEvents`](macro@crate::StateTreeEvents).
/// Each enum variant contains one concrete payload and carries a
/// `#[state_tree(name = "...")]` attribute. Add `broadcast` to variants written
/// through [`BroadcastStateTreeEventWriter`](crate::BroadcastStateTreeEventWriter):
///
/// ```
/// # use bevy_ecs::prelude::*;
/// # use bevy_state_tree::prelude::*;
/// struct Simulation;
///
/// #[derive(EntityEvent)]
/// struct Damage {
///     entity: Entity,
///     amount: u32,
/// }
///
/// #[derive(Event)]
/// struct ResetAll;
///
/// #[derive(StateTreeEvents)]
/// #[state_tree(domain = Simulation)]
/// enum SimulationEvents {
///     #[state_tree(name = "combat.damage")]
///     Damage(Damage),
///     #[state_tree(name = "simulation.reset", broadcast)]
///     ResetAll(ResetAll),
/// }
/// ```
pub trait StateTreeEvents: Send + Sync + 'static {
    /// Returns the SCXML event name represented by this value.
    fn authored_name(&self) -> &'static str;
}

/// Associates one concrete payload type with a domain event variant.
///
/// Implementations are generated with [`StateTreeEvents`](macro@crate::StateTreeEvents) and
/// provide the typed projection needed by planned guards and actions without runtime downcasting.
pub trait StateTreeEventPayload<E: StateTreeEvents>: Event {
    /// SCXML event name associated with this payload type.
    const AUTHORED_NAME: &'static str;

    /// Wraps this payload in its domain event variant.
    fn into_state_tree_event(self) -> E;

    /// Borrows this payload when `event` contains its associated variant.
    fn payload(event: &E) -> Option<&Self>;
}

/// Marks a payload as an entity-targeted event in `E`.
pub trait EntityStateTreeEvent<E: StateTreeEvents>: StateTreeEventPayload<E> + EntityEvent {}

/// Marks a payload as a broadcast event in `E`.
pub trait BroadcastStateTreeEvent<E: StateTreeEvents>: StateTreeEventPayload<E> {}
