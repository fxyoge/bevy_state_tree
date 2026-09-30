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
    StateTreeDomain, StateTreeEffectContext, StateTreeEffectError, StateTreeEventDataError,
    StateTreeInstance,
    definition::ActivityId,
    event::{StateTreeEvent, StateTreeEventRef},
};

/// Portable generation-safe token used by headless activity sinks.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StateTreeHeadlessActivityToken {
    pub(crate) runtime_id: u64,
    pub(crate) activity: ActivityId,
    pub(crate) generation: u64,
}

/// One portable activity start emitted by headless execution.
pub struct StateTreeActivityRequest<'a, D: StateTreeDomain> {
    id: &'a str,
    name: &'a str,
    token: StateTreeHeadlessActivityToken,
    span_start: usize,
    event_name: Option<&'a str>,
    external_event: Option<StateTreeEventRef<'a, D>>,
    data_names: &'a [Box<str>],
    data: &'a [LuauValue],
    arguments: &'a mut StateTreeActivityArgs,
}

impl<'a, D: StateTreeDomain> StateTreeActivityRequest<'a, D> {
    pub fn id(&self) -> &str {
        self.id
    }
    pub fn name(&self) -> &str {
        self.name
    }
    pub fn token(&self) -> StateTreeHeadlessActivityToken {
        self.token
    }
    pub fn span_start(&self) -> usize {
        self.span_start
    }
    pub fn event_name(&self) -> Option<&str> {
        self.event_name
    }
    pub fn external_event(&self) -> Option<StateTreeEventRef<'a, D>> {
        self.external_event
    }
    pub fn event_ref(&self) -> Option<StateTreeEventRef<'a, D>> {
        self.external_event
    }
    pub fn event<T: StateTreeEvent<D>>(&self) -> Option<&T> {
        self.external_event
            .and_then(StateTreeEventRef::payload::<T>)
    }
    /// Returns one instance-local datamodel value.
    pub fn data(&self, name: &str) -> Option<&LuauValue> {
        self.data_names
            .iter()
            .position(|candidate| candidate.as_ref() == name)
            .and_then(|index| self.data.get(index))
    }
    pub fn arguments(&mut self) -> &mut StateTreeActivityArgs {
        self.arguments
    }
}

/// Receives starts and cancellations from a headless state-tree execution.
pub trait StateTreeActivitySink<D: StateTreeDomain> {
    fn start(
        &mut self,
        request: StateTreeActivityRequest<'_, D>,
    ) -> Result<(), StateTreeActivityError>;
    fn cancel(&mut self, token: StateTreeHeadlessActivityToken);
}

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
pub struct StateTreeActivityToken<D, A> {
    pub(crate) entity: Entity,
    pub(crate) runtime_id: u64,
    pub(crate) activity: ActivityId,
    pub(crate) generation: u64,
    marker: PhantomData<fn() -> (D, A)>,
}

impl<D, A> Copy for StateTreeActivityToken<D, A> {}
impl<D, A> Clone for StateTreeActivityToken<D, A> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<D, A> std::fmt::Debug for StateTreeActivityToken<D, A> {
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

impl<D, A> PartialEq for StateTreeActivityToken<D, A> {
    fn eq(&self, other: &Self) -> bool {
        self.entity == other.entity
            && self.runtime_id == other.runtime_id
            && self.activity == other.activity
            && self.generation == other.generation
    }
}
impl<D, A> Eq for StateTreeActivityToken<D, A> {}
impl<D, A> std::hash::Hash for StateTreeActivityToken<D, A> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.entity.hash(state);
        self.runtime_id.hash(state);
        self.activity.hash(state);
        self.generation.hash(state);
    }
}
impl<D, A> PartialOrd for StateTreeActivityToken<D, A> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl<D, A> Ord for StateTreeActivityToken<D, A> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.entity, self.runtime_id, self.activity, self.generation).cmp(&(
            other.entity,
            other.runtime_id,
            other.activity,
            other.generation,
        ))
    }
}

impl<D, A> StateTreeActivityToken<D, A> {
    pub fn entity(self) -> Entity {
        self.entity
    }
}

/// Typed lifecycle messages for one registered activity request type.
pub enum StateTreeActivityLifecycle<D, A: StateTreeActivity> {
    Started {
        token: StateTreeActivityToken<D, A>,
        activity: A,
    },
    Cancelled {
        token: StateTreeActivityToken<D, A>,
    },
}

impl<D: Send + Sync + 'static, A: StateTreeActivity> Message for StateTreeActivityLifecycle<D, A> {}

pub(crate) trait RegisteredActivity<D: StateTreeDomain>: Send + Sync {
    fn start(
        &self,
        context: StateTreeEffectContext<'_, D>,
        arguments: StateTreeActivityArgs,
        token: ErasedActivityToken,
        commands: &ParallelCommands<'_, '_>,
    ) -> Result<(), StateTreeEffectError>;
    fn cancel(&self, token: ErasedActivityToken, commands: &ParallelCommands<'_, '_>);
    fn cancel_deferred(&self, token: ErasedActivityToken, world: &mut DeferredWorld<'_>);
}

pub(crate) trait RuntimeActivitySink<D: StateTreeDomain> {
    fn start(&mut self, request: RuntimeActivityStart<'_, D>) -> Result<(), StateTreeEffectError>;
    fn cancel(
        &mut self,
        handler: Option<&Arc<dyn RegisteredActivity<D>>>,
        token: ErasedActivityToken,
    );
}

pub(crate) struct RuntimeActivityStart<'a, D: StateTreeDomain> {
    pub id: &'a str,
    pub name: &'a str,
    pub handler: Option<&'a Arc<dyn RegisteredActivity<D>>>,
    pub context: StateTreeEffectContext<'a, D>,
    pub arguments: StateTreeActivityArgs,
    pub token: ErasedActivityToken,
    pub span_start: usize,
}

pub(crate) struct RejectActivities;

impl<D: StateTreeDomain> RuntimeActivitySink<D> for RejectActivities {
    fn start(&mut self, request: RuntimeActivityStart<'_, D>) -> Result<(), StateTreeEffectError> {
        Err(StateTreeEffectError::new(format!(
            "activity `{}` requires an activity sink",
            request.name
        )))
    }
    fn cancel(
        &mut self,
        _handler: Option<&Arc<dyn RegisteredActivity<D>>>,
        _token: ErasedActivityToken,
    ) {
    }
}

pub(crate) struct BevyActivitySink<'a, 'w, 's> {
    pub commands: &'a ParallelCommands<'w, 's>,
}

impl<D: StateTreeDomain> RuntimeActivitySink<D> for BevyActivitySink<'_, '_, '_> {
    fn start(&mut self, request: RuntimeActivityStart<'_, D>) -> Result<(), StateTreeEffectError> {
        request
            .handler
            .ok_or_else(|| {
                StateTreeEffectError::new(format!(
                    "activity `{name}` is not registered for this domain",
                    name = request.name
                ))
            })?
            .start(
                request.context,
                request.arguments,
                request.token,
                self.commands,
            )
    }
    fn cancel(
        &mut self,
        handler: Option<&Arc<dyn RegisteredActivity<D>>>,
        token: ErasedActivityToken,
    ) {
        if let Some(handler) = handler {
            handler.cancel(token, self.commands);
        }
    }
}

pub(crate) struct HeadlessActivitySink<'a, S: ?Sized> {
    pub sink: &'a mut S,
}

impl<D, S> RuntimeActivitySink<D> for HeadlessActivitySink<'_, S>
where
    D: StateTreeDomain,
    S: StateTreeActivitySink<D> + ?Sized,
{
    fn start(&mut self, request: RuntimeActivityStart<'_, D>) -> Result<(), StateTreeEffectError> {
        let mut arguments = request.arguments;
        self.sink.start(StateTreeActivityRequest {
            id: request.id,
            name: request.name,
            token: StateTreeHeadlessActivityToken {
                runtime_id: request.token.runtime_id,
                activity: request.token.activity,
                generation: request.token.generation,
            },
            span_start: request.span_start,
            event_name: request.context.event_name,
            external_event: request.context.external_event,
            data_names: request.context.data_names,
            data: request.context.data,
            arguments: &mut arguments,
        })
    }
    fn cancel(
        &mut self,
        _handler: Option<&Arc<dyn RegisteredActivity<D>>>,
        token: ErasedActivityToken,
    ) {
        self.sink.cancel(StateTreeHeadlessActivityToken {
            runtime_id: token.runtime_id,
            activity: token.activity,
            generation: token.generation,
        });
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
    fn typed<D, A>(self) -> StateTreeActivityToken<D, A> {
        StateTreeActivityToken {
            entity: self.entity,
            runtime_id: self.runtime_id,
            activity: self.activity,
            generation: self.generation,
            marker: PhantomData,
        }
    }
}

struct TypedActivity<D, A, F> {
    constructor: F,
    marker: PhantomData<fn() -> (D, A)>,
}

impl<D, A, F> RegisteredActivity<D> for TypedActivity<D, A, F>
where
    D: StateTreeDomain,
    A: StateTreeActivity,
    F: Fn(
            StateTreeActivityContext<'_, D>,
            &mut StateTreeActivityArgs,
        ) -> Result<A, StateTreeActivityError>
        + Send
        + Sync
        + 'static,
{
    fn start(
        &self,
        context: StateTreeEffectContext<'_, D>,
        mut arguments: StateTreeActivityArgs,
        token: ErasedActivityToken,
        commands: &ParallelCommands<'_, '_>,
    ) -> Result<(), StateTreeEffectError> {
        let activity = (self.constructor)(context, &mut arguments)?;
        arguments.finish()?;
        commands.command_scope(move |mut commands| {
            commands.write_message(StateTreeActivityLifecycle::<D, A>::Started {
                token: token.typed(),
                activity,
            });
        });
        Ok(())
    }

    fn cancel(&self, token: ErasedActivityToken, commands: &ParallelCommands<'_, '_>) {
        commands.command_scope(move |mut commands| {
            commands.write_message(StateTreeActivityLifecycle::<D, A>::Cancelled {
                token: token.typed(),
            });
        });
    }

    fn cancel_deferred(&self, token: ErasedActivityToken, world: &mut DeferredWorld<'_>) {
        world.write_message(StateTreeActivityLifecycle::<D, A>::Cancelled {
            token: token.typed(),
        });
    }
}

#[derive(Resource)]
pub(crate) struct StateTreeActivityRegistry<D: StateTreeDomain> {
    activities: HashMap<Box<str>, Arc<dyn RegisteredActivity<D>>>,
    revision: u64,
}

impl<D: StateTreeDomain> Default for StateTreeActivityRegistry<D> {
    fn default() -> Self {
        Self {
            activities: HashMap::new(),
            revision: 0,
        }
    }
}

impl<D: StateTreeDomain> StateTreeActivityRegistry<D> {
    pub(crate) fn get(&self, name: &str) -> Option<Arc<dyn RegisteredActivity<D>>> {
        self.activities.get(name).cloned()
    }
    pub(crate) fn revision(&self) -> u64 {
        self.revision
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
pub(crate) struct StateTreeActivityInbox<D: StateTreeDomain> {
    pub outcomes: Mutex<Vec<ActivityOutcome>>,
    marker: PhantomData<fn() -> D>,
}

impl<D: StateTreeDomain> Default for StateTreeActivityInbox<D> {
    fn default() -> Self {
        Self {
            outcomes: Mutex::new(Vec::new()),
            marker: PhantomData,
        }
    }
}

#[derive(Resource)]
pub(crate) struct StateTreeActivityIngress<D: StateTreeDomain> {
    pub dirty: Mutex<bevy_ecs::entity::EntityHashSet>,
    pub dirty_hint: AtomicBool,
    marker: PhantomData<fn() -> D>,
}

impl<D: StateTreeDomain> Default for StateTreeActivityIngress<D> {
    fn default() -> Self {
        Self {
            dirty: Mutex::new(bevy_ecs::entity::EntityHashSet::default()),
            dirty_hint: AtomicBool::new(false),
            marker: PhantomData,
        }
    }
}

/// Writes terminal outcomes for one typed activity request.
#[derive(SystemParam)]
pub struct StateTreeActivityOutcomeWriter<'w, 's, D: StateTreeDomain, A: StateTreeActivity> {
    ingress: Res<'w, StateTreeActivityIngress<D>>,
    instances: Query<
        'w,
        's,
        (
            &'static StateTreeInstance<D>,
            &'static StateTreeActivityInbox<D>,
        ),
    >,
    marker: PhantomData<A>,
}

impl<D: StateTreeDomain, A: StateTreeActivity> StateTreeActivityOutcomeWriter<'_, '_, D, A> {
    /// Projects and enqueues a completion when `token` still names an active run.
    ///
    /// Returns `true` only when the outcome was accepted and enqueued.
    pub fn complete(&mut self, token: StateTreeActivityToken<D, A>, output: A::Output) -> bool {
        self.write(token, ActivityOutcomeKind::Complete, || {
            A::output_data(&output)
        })
    }

    /// Projects and enqueues a failure when `token` still names an active run.
    ///
    /// Returns `true` only when the outcome was accepted and enqueued.
    pub fn fail(&mut self, token: StateTreeActivityToken<D, A>, error: A::Error) -> bool {
        self.write(token, ActivityOutcomeKind::Fail, || A::error_data(&error))
    }

    fn write<F>(
        &mut self,
        token: StateTreeActivityToken<D, A>,
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

pub(crate) fn register_activity<D, A, F>(app: &mut App, name: Box<str>, constructor: F)
where
    D: StateTreeDomain,
    A: StateTreeActivity,
    F: Fn(
            StateTreeActivityContext<'_, D>,
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
    app.add_message::<StateTreeActivityLifecycle<D, A>>();
    app.init_resource::<StateTreeActivityRegistry<D>>();
    let mut registry = app
        .world_mut()
        .resource_mut::<StateTreeActivityRegistry<D>>();
    assert!(
        !registry.activities.contains_key(&name),
        "state-tree activity `{name}` was registered more than once"
    );
    registry.activities.insert(
        name.clone(),
        Arc::new(TypedActivity::<D, A, F> {
            constructor,
            marker: PhantomData,
        }),
    );
    registry.revision = registry.revision.wrapping_add(1);
}
