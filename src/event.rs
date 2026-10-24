use std::{
    any::{Any, TypeId, type_name},
    collections::HashMap,
    sync::{Arc, OnceLock},
};

use bevy_app::App;
use bevy_ecs::event::{EntityEvent, Event};
use thiserror::Error;

use crate::{LuauValue, protocol::RegistrationOrigin};

/// An error while projecting a typed external event for Luau `_event.data`.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("{message}")]
pub struct StateTreeEventDataError {
    message: Box<str>,
}

impl StateTreeEventDataError {
    pub fn new(message: impl Into<Box<str>>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// One external event payload accepted by state trees.
///
/// Implementations are normally generated with
/// [`StateTreeEvent`](macro@crate::StateTreeEvent).
pub trait StateTreeEvent: Event {
    /// The SCXML event name associated with this payload type.
    const AUTHORED_NAME: &'static str;

    /// Projects this event's typed payload for lazy Luau `_event.data` access.
    fn event_data(&self) -> Result<Option<LuauValue>, StateTreeEventDataError> {
        Ok(None)
    }
}

/// Marks a payload as an entity-targeted state-tree event.
pub trait EntityStateTreeEvent: StateTreeEvent + EntityEvent {}

/// Marks a payload as a broadcast state-tree event.
pub trait BroadcastStateTreeEvent: StateTreeEvent {}

/// Invalid state-tree event protocol registration.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub(crate) enum StateTreeEventRegistrationError {
    /// The same Rust payload type was registered more than once.
    #[error("state-tree event payload type `{type_name}` is already registered")]
    DuplicateType { type_name: &'static str },
    /// Authored event names must contain at least one character.
    #[error("state-tree event payload type `{type_name}` has an empty authored name")]
    EmptyName { type_name: &'static str },
    /// One authored event name cannot identify multiple Rust payload types.
    #[error(
        "state-tree event name `{name}` is already registered for payload type `{registered_type_name}`"
    )]
    DuplicateName {
        name: &'static str,
        registered_type_name: &'static str,
    },
}

/// Builds the application's external event protocol.
pub(crate) struct StateTreeEventRegistry {
    entries: Vec<RegisteredEvent>,
    type_ids: HashMap<TypeId, RegisteredEventId>,
    names: HashMap<&'static str, &'static str>,
    frozen: OnceLock<StateTreeEventProtocol>,
}

#[derive(bevy_ecs::prelude::Resource, Default)]
pub(crate) struct StateTreeAppEventRegistry(StateTreeEventRegistry);

impl StateTreeAppEventRegistry {
    pub(crate) fn register<E>(
        &mut self,
        origin: RegistrationOrigin,
    ) -> Result<(), StateTreeEventRegistrationError>
    where
        E: StateTreeEvent,
    {
        self.0.register_with_origin::<E>(origin).map(|_| ())
    }

    pub(crate) fn protocol(&self) -> &StateTreeEventProtocol {
        self.0.protocol()
    }

    pub(crate) fn registrations(&self) -> impl Iterator<Item = (&str, RegistrationOrigin)> {
        self.0
            .entries
            .iter()
            .map(|entry| (entry.authored_name, entry.origin))
    }

    pub(crate) fn ensure<E>(
        &mut self,
        origin: RegistrationOrigin,
    ) -> Result<(), StateTreeEventRegistrationError>
    where
        E: StateTreeEvent,
    {
        if self.0.type_ids.contains_key(&TypeId::of::<E>()) {
            Ok(())
        } else {
            self.register::<E>(origin)
        }
    }
}

impl Default for StateTreeEventRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl StateTreeEventRegistry {
    /// Creates an empty event protocol builder.
    pub(crate) fn new() -> Self {
        Self {
            entries: Vec::new(),
            type_ids: HashMap::new(),
            names: HashMap::new(),
            frozen: OnceLock::new(),
        }
    }

    /// Registers one concrete event payload in insertion order.
    #[cfg(test)]
    pub(crate) fn register<E>(&mut self) -> Result<&mut Self, StateTreeEventRegistrationError>
    where
        E: StateTreeEvent,
    {
        self.register_with_origin::<E>(RegistrationOrigin::Application)
    }

    fn register_with_origin<E>(
        &mut self,
        origin: RegistrationOrigin,
    ) -> Result<&mut Self, StateTreeEventRegistrationError>
    where
        E: StateTreeEvent,
    {
        if E::AUTHORED_NAME.is_empty() {
            return Err(StateTreeEventRegistrationError::EmptyName {
                type_name: type_name::<E>(),
            });
        }
        let type_id = TypeId::of::<E>();
        if self.type_ids.contains_key(&type_id) {
            return Err(StateTreeEventRegistrationError::DuplicateType {
                type_name: type_name::<E>(),
            });
        }
        if let Some(registered_type_name) = self.names.get(E::AUTHORED_NAME) {
            return Err(StateTreeEventRegistrationError::DuplicateName {
                name: E::AUTHORED_NAME,
                registered_type_name,
            });
        }

        let id = RegisteredEventId(self.entries.len());
        self.type_ids.insert(type_id, id);
        self.names.insert(E::AUTHORED_NAME, type_name::<E>());
        self.entries.push(RegisteredEvent {
            id,
            type_id,
            authored_name: E::AUTHORED_NAME,
            origin,
            event_data: project_event_data::<E>,
        });
        Ok(self)
    }

    /// Freezes this builder into an immutable, cheaply cloneable protocol.
    #[cfg(test)]
    pub(crate) fn freeze(&self) -> StateTreeEventProtocol {
        self.protocol().clone()
    }

    pub(crate) fn protocol(&self) -> &StateTreeEventProtocol {
        self.frozen.get_or_init(|| {
            let authored_names = self
                .entries
                .iter()
                .map(|entry| entry.authored_name)
                .collect();
            StateTreeEventProtocol {
                inner: Arc::new(StateTreeEventProtocolInner {
                    entries: self.entries.clone().into(),
                    authored_names,
                    type_ids: self.type_ids.clone(),
                }),
            }
        })
    }
}

/// The immutable application-wide external event protocol.
pub(crate) struct StateTreeEventProtocol {
    inner: Arc<StateTreeEventProtocolInner>,
}

pub(crate) fn register_event<E>(app: &mut App, origin: RegistrationOrigin)
where
    E: StateTreeEvent,
{
    let result = app
        .world_mut()
        .resource_mut::<StateTreeAppEventRegistry>()
        .ensure::<E>(origin);
    if let Err(error) = result {
        panic!(
            "failed to register state-tree event `{}`: {error}",
            type_name::<E>()
        );
    }
}

pub(crate) fn ensure_event<E>(app: &mut App, origin: RegistrationOrigin)
where
    E: StateTreeEvent,
{
    let result = app
        .world_mut()
        .resource_mut::<StateTreeAppEventRegistry>()
        .ensure::<E>(origin);
    if let Err(error) = result {
        panic!(
            "failed to register state-tree event `{}`: {error}",
            type_name::<E>()
        );
    }
}

impl Clone for StateTreeEventProtocol {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl StateTreeEventProtocol {
    pub(crate) fn registered_event_id(&self, type_id: TypeId) -> Option<RegisteredEventId> {
        self.inner.type_ids.get(&type_id).copied()
    }

    pub(crate) fn authored_names(&self) -> &[&'static str] {
        &self.inner.authored_names
    }

    pub(crate) fn entry(&self, id: RegisteredEventId) -> &RegisteredEvent {
        &self.inner.entries[id.index()]
    }

    pub(crate) fn queue<E>(&self, payload: E) -> Option<QueuedStateTreeEvent>
    where
        E: StateTreeEvent,
    {
        let id = self.registered_event_id(TypeId::of::<E>())?;
        Some(QueuedStateTreeEvent::new(id, payload))
    }

    pub(crate) fn event_ref<'a>(
        &'a self,
        event: &'a QueuedStateTreeEvent,
    ) -> StateTreeEventRef<'a> {
        let entry = self.entry(event.id);
        debug_assert_eq!(entry.type_id, event.payload.as_ref().type_id());
        StateTreeEventRef {
            entry,
            payload: event.payload.as_ref(),
        }
    }

    #[cfg(test)]
    pub(crate) fn borrowed_event<'a, E>(&'a self, event: &'a E) -> Option<StateTreeEventRef<'a>>
    where
        E: StateTreeEvent,
    {
        let id = self.registered_event_id(TypeId::of::<E>())?;
        Some(StateTreeEventRef {
            entry: self.entry(id),
            payload: event,
        })
    }
}

struct StateTreeEventProtocolInner {
    entries: Box<[RegisteredEvent]>,
    authored_names: Box<[&'static str]>,
    type_ids: HashMap<TypeId, RegisteredEventId>,
}

/// A typed borrowed view of one queued state-tree event.
pub struct StateTreeEventRef<'a> {
    entry: &'a RegisteredEvent,
    payload: &'a (dyn Any + Send + Sync),
}

impl Clone for StateTreeEventRef<'_> {
    fn clone(&self) -> Self {
        *self
    }
}

impl Copy for StateTreeEventRef<'_> {}

impl<'a> StateTreeEventRef<'a> {
    /// Returns the event name used by authored state-tree definitions.
    pub fn authored_name(self) -> &'static str {
        self.entry.authored_name
    }

    /// Borrows the payload when this event contains `T`.
    pub fn payload<T: StateTreeEvent>(self) -> Option<&'a T> {
        self.payload.downcast_ref()
    }

    /// Returns whether this event contains a `T` payload.
    pub fn is<T: StateTreeEvent>(self) -> bool {
        self.payload.is::<T>()
    }

    pub(crate) fn event_data(self) -> Result<Option<LuauValue>, StateTreeEventDataError> {
        (self.entry.event_data)(self.payload)
    }

    pub(crate) fn registered_event_id(self) -> RegisteredEventId {
        self.entry.id
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct RegisteredEventId(usize);

impl RegisteredEventId {
    pub(crate) fn index(self) -> usize {
        self.0
    }
}

#[derive(Clone)]
pub(crate) struct RegisteredEvent {
    id: RegisteredEventId,
    type_id: TypeId,
    authored_name: &'static str,
    pub(crate) origin: RegistrationOrigin,
    event_data: EventDataProjector,
}

/// The ownership strategy is deliberately private. `event_storage` benchmarks
/// show the allocation cost directly; a 16-byte inline prototype increased the
/// queue stride and did not improve end-to-end entity-event throughput.
pub(crate) struct QueuedStateTreeEvent {
    id: RegisteredEventId,
    payload: Box<dyn Any + Send + Sync>,
}

impl QueuedStateTreeEvent {
    pub(crate) fn new<E: StateTreeEvent>(id: RegisteredEventId, payload: E) -> Self {
        Self {
            id,
            payload: Box::new(payload),
        }
    }

    pub(crate) fn view<'a>(
        &'a self,
        protocol: &'a StateTreeEventProtocol,
    ) -> StateTreeEventRef<'a> {
        protocol.event_ref(self)
    }

    #[cfg(test)]
    pub(crate) fn project<T: StateTreeEvent>(&self) -> Option<&T> {
        self.payload.downcast_ref()
    }
}

type EventDataProjector =
    fn(&(dyn Any + Send + Sync)) -> Result<Option<LuauValue>, StateTreeEventDataError>;

fn project_event_data<E>(
    payload: &(dyn Any + Send + Sync),
) -> Result<Option<LuauValue>, StateTreeEventDataError>
where
    E: StateTreeEvent,
{
    let payload = payload.downcast_ref::<E>().ok_or_else(|| {
        StateTreeEventDataError::new(format!(
            "registered event payload did not contain `{}`",
            type_name::<E>()
        ))
    })?;
    payload.event_data()
}

#[cfg(test)]
mod tests {
    use std::{
        any::{TypeId, type_name},
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use bevy_ecs::event::Event;

    use super::*;

    #[derive(Event)]
    struct First(u32);

    impl StateTreeEvent for First {
        const AUTHORED_NAME: &'static str = "first";
    }

    #[derive(Event)]
    struct Second(&'static str);

    impl StateTreeEvent for Second {
        const AUTHORED_NAME: &'static str = "second";
    }

    #[derive(Event)]
    struct SameName;

    impl StateTreeEvent for SameName {
        const AUTHORED_NAME: &'static str = "first";
    }

    #[derive(Event)]
    struct Unregistered;

    impl StateTreeEvent for Unregistered {
        const AUTHORED_NAME: &'static str = "unregistered";
    }

    #[derive(Event)]
    struct EmptyName;

    impl StateTreeEvent for EmptyName {
        const AUTHORED_NAME: &'static str = "";
    }

    static PROJECTIONS: AtomicUsize = AtomicUsize::new(0);

    #[derive(Event)]
    struct Projected(i64);

    impl StateTreeEvent for Projected {
        const AUTHORED_NAME: &'static str = "projected";

        fn event_data(&self) -> Result<Option<LuauValue>, StateTreeEventDataError> {
            PROJECTIONS.fetch_add(1, Ordering::Relaxed);
            Ok(Some(LuauValue::Integer(self.0)))
        }
    }

    #[test]
    fn registration_order_produces_dense_usable_dispatch() {
        let mut registry = StateTreeEventRegistry::new();
        registry
            .register::<First>()
            .unwrap()
            .register::<Second>()
            .unwrap();
        let protocol = registry.freeze();

        assert_eq!(protocol.authored_names(), &["first", "second"]);
        assert_eq!(
            protocol.registered_event_id(TypeId::of::<First>()),
            Some(RegisteredEventId(0))
        );
        assert_eq!(
            protocol.registered_event_id(TypeId::of::<Second>()),
            Some(RegisteredEventId(1))
        );

        let first = protocol.queue(First(7)).unwrap();
        let second = protocol.queue(Second("payload")).unwrap();
        let first = first.view(&protocol);
        let second = second.view(&protocol);
        assert_eq!(first.registered_event_id().index(), 0);
        assert_eq!(first.payload::<First>().unwrap().0, 7);
        assert_eq!(second.registered_event_id().index(), 1);
        assert_eq!(second.payload::<Second>().unwrap().0, "payload");
    }

    #[test]
    fn duplicate_rust_type_is_rejected() {
        let mut registry = StateTreeEventRegistry::new();
        registry.register::<First>().unwrap();

        assert_eq!(
            registry.register::<First>().err().unwrap(),
            StateTreeEventRegistrationError::DuplicateType {
                type_name: type_name::<First>(),
            }
        );
    }

    #[test]
    fn duplicate_authored_names_are_rejected() {
        let mut registry = StateTreeEventRegistry::new();
        registry.register::<First>().unwrap();

        assert_eq!(
            registry.register::<SameName>().err().unwrap(),
            StateTreeEventRegistrationError::DuplicateName {
                name: "first",
                registered_type_name: type_name::<First>(),
            }
        );
    }

    #[test]
    fn ensuring_the_same_event_type_is_idempotent() {
        let mut registry = StateTreeAppEventRegistry::default();
        registry
            .ensure::<First>(RegistrationOrigin::Application)
            .unwrap();
        registry
            .ensure::<First>(RegistrationOrigin::Application)
            .unwrap();

        assert_eq!(registry.protocol().authored_names(), &["first"]);
    }

    #[test]
    fn freeze_is_idempotent_and_cloneable() {
        let mut registry = StateTreeEventRegistry::new();
        registry.register::<First>().unwrap();

        let first = registry.freeze();
        let second = registry.freeze();
        let cloned = first.clone();
        assert!(Arc::ptr_eq(&first.inner, &second.inner));
        assert!(Arc::ptr_eq(&first.inner, &cloned.inner));
    }

    #[test]
    fn empty_manual_authored_name_is_rejected() {
        let mut registry = StateTreeEventRegistry::new();

        assert_eq!(
            registry.register::<EmptyName>().err().unwrap(),
            StateTreeEventRegistrationError::EmptyName {
                type_name: type_name::<EmptyName>(),
            }
        );
    }

    #[test]
    fn borrowed_typed_views_downcast_correctly() {
        let mut registry = StateTreeEventRegistry::new();
        registry
            .register::<First>()
            .unwrap()
            .register::<Second>()
            .unwrap();
        let protocol = registry.freeze();
        let payload = First(42);

        let view = protocol.borrowed_event(&payload).unwrap();
        assert_eq!(view.authored_name(), "first");
        assert!(view.is::<First>());
        assert!(!view.is::<Second>());
        assert_eq!(view.payload::<First>().unwrap().0, 42);
        assert!(view.payload::<Second>().is_none());
    }

    #[test]
    fn unregistered_lookup_and_queue_are_rejected() {
        let protocol = StateTreeEventRegistry::new().freeze();
        let payload = Unregistered;

        assert_eq!(
            protocol.registered_event_id(TypeId::of::<Unregistered>()),
            None
        );
        assert!(protocol.borrowed_event(&payload).is_none());
        assert!(protocol.queue(payload).is_none());
    }

    #[test]
    fn event_data_projector_is_lazy() {
        PROJECTIONS.store(0, Ordering::Relaxed);
        let mut registry = StateTreeEventRegistry::new();
        registry.register::<Projected>().unwrap();
        let protocol = registry.freeze();
        let queued = protocol.queue(Projected(23)).unwrap();
        let view = queued.view(&protocol);

        assert_eq!(PROJECTIONS.load(Ordering::Relaxed), 0);
        assert_eq!(view.authored_name(), "projected");
        assert!(view.is::<Projected>());
        assert_eq!(PROJECTIONS.load(Ordering::Relaxed), 0);
        assert_eq!(view.event_data().unwrap(), Some(LuauValue::Integer(23)));
        assert_eq!(PROJECTIONS.load(Ordering::Relaxed), 1);
    }
}
