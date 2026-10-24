use std::{
    collections::HashMap,
    hash::Hash,
    marker::PhantomData,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use bevy_app::App;
use bevy_ecs::{
    entity::Entity,
    message::Message,
    prelude::{Component, Query, Res, Resource},
    system::{ParallelCommands, SystemParam},
    world::DeferredWorld,
};

use crate::{
    LuauValue, StateTreeActivityArgs, StateTreeActivityContext, StateTreeActivityError,
    StateTreeEffectContext, StateTreeEffectError, StateTreeEventDataError, StateTreeInstance,
    definition::ActivityId, protocol::RegistrationOrigin,
};

/// A typed long-running request owned by one active state.
pub trait StateTreeActivity: Send + Sync + 'static {
    type Output: Send + Sync + 'static;
    type Error: Send + Sync + 'static;

    fn output_data(_output: &Self::Output) -> Result<Option<LuauValue>, StateTreeEventDataError> {
        Ok(None)
    }

    fn error_data(_error: &Self::Error) -> Result<Option<LuauValue>, StateTreeEventDataError> {
        Ok(None)
    }
}

/// A generation-safe handle to one activity run on one entity.
pub struct StateTreeActivityToken<A> {
    pub(crate) entity: Entity,
    pub(crate) runtime_id: u64,
    pub(crate) activity: ActivityId,
    pub(crate) generation: u64,
    marker: PhantomData<fn() -> A>,
}

impl<A> Copy for StateTreeActivityToken<A> {}
impl<A> Clone for StateTreeActivityToken<A> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<A> std::fmt::Debug for StateTreeActivityToken<A> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StateTreeActivityToken")
            .field("entity", &self.entity)
            .field("runtime_id", &self.runtime_id)
            .field("activity", &self.activity)
            .field("generation", &self.generation)
            .finish()
    }
}

impl<A> PartialEq for StateTreeActivityToken<A> {
    fn eq(&self, other: &Self) -> bool {
        self.entity == other.entity
            && self.runtime_id == other.runtime_id
            && self.activity == other.activity
            && self.generation == other.generation
    }
}
impl<A> Eq for StateTreeActivityToken<A> {}
impl<A> std::hash::Hash for StateTreeActivityToken<A> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.entity.hash(state);
        self.runtime_id.hash(state);
        self.activity.hash(state);
        self.generation.hash(state);
    }
}
impl<A> PartialOrd for StateTreeActivityToken<A> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl<A> Ord for StateTreeActivityToken<A> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.entity, self.runtime_id, self.activity, self.generation).cmp(&(
            other.entity,
            other.runtime_id,
            other.activity,
            other.generation,
        ))
    }
}

impl<A> StateTreeActivityToken<A> {
    pub fn entity(self) -> Entity {
        self.entity
    }

    pub(crate) fn erased(self) -> ErasedActivityToken {
        ErasedActivityToken {
            entity: self.entity,
            runtime_id: self.runtime_id,
            activity: self.activity,
            generation: self.generation,
        }
    }
}

/// Typed lifecycle messages for one registered activity request type.
pub enum StateTreeActivityLifecycle<A: StateTreeActivity> {
    Started {
        token: StateTreeActivityToken<A>,
        activity: A,
    },
    Cancelled {
        token: StateTreeActivityToken<A>,
    },
}

impl<A: StateTreeActivity> Message for StateTreeActivityLifecycle<A> {}

pub(crate) trait RegisteredActivity: Send + Sync {
    #[cfg(feature = "persistence")]
    fn discard_pending_starts(&self, world: &mut bevy_ecs::world::World);
    fn start(
        &self,
        context: StateTreeEffectContext<'_>,
        arguments: StateTreeActivityArgs,
        token: ErasedActivityToken,
        commands: &ParallelCommands<'_, '_>,
    ) -> Result<(), StateTreeEffectError>;
    fn cancel(&self, token: ErasedActivityToken, commands: &ParallelCommands<'_, '_>);
    fn cancel_deferred(&self, token: ErasedActivityToken, world: &mut DeferredWorld<'_>);
}

pub(crate) trait RuntimeActivitySink {
    fn start(&mut self, request: RuntimeActivityStart<'_>) -> Result<(), StateTreeEffectError>;
    fn cancel(&mut self, handler: &Arc<dyn RegisteredActivity>, token: ErasedActivityToken);
}

pub(crate) struct RuntimeActivityStart<'a> {
    pub handler: &'a Arc<dyn RegisteredActivity>,
    pub context: StateTreeEffectContext<'a>,
    pub arguments: StateTreeActivityArgs,
    pub token: ErasedActivityToken,
}

pub(crate) struct BevyActivitySink<'a, 'w, 's> {
    pub commands: &'a ParallelCommands<'w, 's>,
}

impl RuntimeActivitySink for BevyActivitySink<'_, '_, '_> {
    fn start(&mut self, request: RuntimeActivityStart<'_>) -> Result<(), StateTreeEffectError> {
        request.handler.start(
            request.context,
            request.arguments,
            request.token,
            self.commands,
        )
    }
    fn cancel(&mut self, handler: &Arc<dyn RegisteredActivity>, token: ErasedActivityToken) {
        handler.cancel(token, self.commands);
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct ErasedActivityToken {
    pub entity: Entity,
    pub runtime_id: u64,
    pub activity: ActivityId,
    pub generation: u64,
}

impl ErasedActivityToken {
    pub(crate) fn typed<A>(self) -> StateTreeActivityToken<A> {
        StateTreeActivityToken {
            entity: self.entity,
            runtime_id: self.runtime_id,
            activity: self.activity,
            generation: self.generation,
            marker: PhantomData,
        }
    }
}

struct TypedActivity<A, F> {
    constructor: F,
    marker: PhantomData<fn() -> A>,
}

impl<A, F> RegisteredActivity for TypedActivity<A, F>
where
    A: StateTreeActivity,
    F: Fn(
            StateTreeActivityContext<'_>,
            &mut StateTreeActivityArgs,
        ) -> Result<A, StateTreeActivityError>
        + Send
        + Sync
        + 'static,
{
    #[cfg(feature = "persistence")]
    fn discard_pending_starts(&self, world: &mut bevy_ecs::world::World) {
        let mut messages =
            world.resource_mut::<bevy_ecs::message::Messages<StateTreeActivityLifecycle<A>>>();
        let cancellations: Vec<_> = messages
            .drain()
            .filter(|message| matches!(message, StateTreeActivityLifecycle::Cancelled { .. }))
            .collect();
        messages.write_batch(cancellations);
    }
    fn start(
        &self,
        context: StateTreeEffectContext<'_>,
        mut arguments: StateTreeActivityArgs,
        token: ErasedActivityToken,
        commands: &ParallelCommands<'_, '_>,
    ) -> Result<(), StateTreeEffectError> {
        let activity = (self.constructor)(context, &mut arguments)?;
        arguments.finish()?;
        commands.command_scope(move |mut commands| {
            commands.write_message(StateTreeActivityLifecycle::<A>::Started {
                token: token.typed(),
                activity,
            });
        });
        Ok(())
    }

    fn cancel(&self, token: ErasedActivityToken, commands: &ParallelCommands<'_, '_>) {
        commands.command_scope(move |mut commands| {
            commands.write_message(StateTreeActivityLifecycle::<A>::Cancelled {
                token: token.typed(),
            });
        });
    }

    fn cancel_deferred(&self, token: ErasedActivityToken, world: &mut DeferredWorld<'_>) {
        world.write_message(StateTreeActivityLifecycle::<A>::Cancelled {
            token: token.typed(),
        });
    }
}

#[derive(Resource, Default)]
pub(crate) struct StateTreeActivityRegistry {
    activities: HashMap<Box<str>, Arc<dyn RegisteredActivity>>,
    origins: HashMap<Box<str>, RegistrationOrigin>,
}

impl StateTreeActivityRegistry {
    #[cfg(feature = "persistence")]
    pub(crate) fn discard_pending_starts(&self, world: &mut bevy_ecs::world::World) {
        for handler in self.activities.values() {
            handler.discard_pending_starts(world);
        }
    }
    pub(crate) fn get(&self, name: &str) -> Option<Arc<dyn RegisteredActivity>> {
        self.activities.get(name).cloned()
    }

    pub(crate) fn registrations(&self) -> impl Iterator<Item = (&str, RegistrationOrigin)> {
        self.origins
            .iter()
            .map(|(name, origin)| (name.as_ref(), *origin))
    }
}

pub(crate) enum ActivityOutcomeKind {
    Complete,
    Fail,
}

pub(crate) struct ActivityOutcome {
    pub token: ErasedActivityToken,
    pub kind: ActivityOutcomeKind,
    pub data: Result<Option<LuauValue>, StateTreeEventDataError>,
}

#[derive(Component)]
pub(crate) struct StateTreeActivityInbox {
    pub outcomes: Mutex<Vec<ActivityOutcome>>,
}

impl Default for StateTreeActivityInbox {
    fn default() -> Self {
        Self {
            outcomes: Mutex::new(Vec::new()),
        }
    }
}

#[derive(Resource)]
pub(crate) struct StateTreeActivityIngress {
    pub dirty: Mutex<bevy_ecs::entity::EntityHashSet>,
    pub dirty_hint: AtomicBool,
}

impl Default for StateTreeActivityIngress {
    fn default() -> Self {
        Self {
            dirty: Mutex::new(bevy_ecs::entity::EntityHashSet::default()),
            dirty_hint: AtomicBool::new(false),
        }
    }
}

/// Writes terminal outcomes for one typed activity request.
#[derive(SystemParam)]
pub struct StateTreeActivityOutcomeWriter<'w, 's, A: StateTreeActivity> {
    ingress: Res<'w, StateTreeActivityIngress>,
    instances: Query<'w, 's, (&'static StateTreeInstance, &'static StateTreeActivityInbox)>,
    marker: PhantomData<A>,
}

impl<A: StateTreeActivity> StateTreeActivityOutcomeWriter<'_, '_, A> {
    /// Projects and enqueues a completion when `token` still names an active run.
    ///
    /// Returns `true` only when the outcome was accepted and enqueued.
    pub fn complete(&mut self, token: StateTreeActivityToken<A>, output: A::Output) -> bool {
        self.write(token, ActivityOutcomeKind::Complete, || {
            A::output_data(&output)
        })
    }

    /// Projects and enqueues a failure when `token` still names an active run.
    ///
    /// Returns `true` only when the outcome was accepted and enqueued.
    pub fn fail(&mut self, token: StateTreeActivityToken<A>, error: A::Error) -> bool {
        self.write(token, ActivityOutcomeKind::Fail, || A::error_data(&error))
    }

    fn write<F>(
        &mut self,
        token: StateTreeActivityToken<A>,
        kind: ActivityOutcomeKind,
        project: F,
    ) -> bool
    where
        F: FnOnce() -> Result<Option<LuauValue>, StateTreeEventDataError>,
    {
        let Ok((instance, inbox)) = self.instances.get(token.entity) else {
            return false;
        };
        let erased = ErasedActivityToken {
            entity: token.entity,
            runtime_id: token.runtime_id,
            activity: token.activity,
            generation: token.generation,
        };
        if !instance.accepts_activity_token(erased) {
            return false;
        }
        let mut outcomes = inbox
            .outcomes
            .lock()
            .expect("state-tree activity inbox poisoned");
        if outcomes.iter().any(|outcome| outcome.token == erased) {
            return false;
        }
        outcomes.push(ActivityOutcome {
            token: erased,
            kind,
            data: project(),
        });
        drop(outcomes);
        self.ingress
            .dirty
            .lock()
            .expect("state-tree activity dirty set poisoned")
            .insert(token.entity);
        self.ingress.dirty_hint.store(true, Ordering::Release);
        true
    }
}

pub(crate) fn register_activity<A, F>(
    app: &mut App,
    name: Box<str>,
    constructor: F,
    origin: RegistrationOrigin,
) where
    A: StateTreeActivity,
    F: Fn(
            StateTreeActivityContext<'_>,
            &mut StateTreeActivityArgs,
        ) -> Result<A, StateTreeActivityError>
        + Send
        + Sync
        + 'static,
{
    assert!(
        !name.is_empty(),
        "state-tree activity names cannot be empty"
    );
    app.add_message::<StateTreeActivityLifecycle<A>>();
    app.init_resource::<StateTreeActivityRegistry>();
    let mut registry = app.world_mut().resource_mut::<StateTreeActivityRegistry>();
    if registry.activities.contains_key(&name) {
        if name.as_ref() == "bst.spawn" {
            panic!(
                "state-tree activity `bst.spawn` is reserved for the built-in state-tree spawner"
            );
        }
        panic!("state-tree activity `{name}` was registered more than once");
    }
    registry.activities.insert(
        name.clone(),
        Arc::new(TypedActivity::<A, F> {
            constructor,
            marker: PhantomData,
        }),
    );
    registry.origins.insert(name, origin);
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestActivity;

    impl StateTreeActivity for TestActivity {
        type Output = ();
        type Error = ();
    }

    #[test]
    #[should_panic(expected = "state-tree activity `duplicate` was registered more than once")]
    fn activity_names_must_be_unique() {
        let mut app = App::new();
        register_activity::<TestActivity, _>(
            &mut app,
            "duplicate".into(),
            |_context, _arguments| Ok(TestActivity),
            RegistrationOrigin::Application,
        );
        register_activity::<TestActivity, _>(
            &mut app,
            "duplicate".into(),
            |_context, _arguments| Ok(TestActivity),
            RegistrationOrigin::Application,
        );
    }
}
