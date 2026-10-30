use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashMap},
    num::NonZeroUsize,
    ops::Range,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use bevy_app::{App, Plugin};
use bevy_asset::{AssetApp, AssetEvent, AssetServer, Assets};
use bevy_ecs::{
    change_detection::{DetectChanges, DetectChangesMut},
    entity::{EntityHashMap, EntityHashSet},
    entity_disabling::Disabled,
    prelude::{
        Changed, Commands, Component, Entity, MessageReader, Mut, Query, RemovedComponents, Res,
        ResMut, Resource, With,
    },
    query::Allow,
    schedule::{
        InternedScheduleLabel, IntoScheduleConfigs, ScheduleLabel, SystemCondition, SystemSet,
        common_conditions::{
            any_component_removed, any_match_filter, on_message, resource_changed,
        },
    },
    system::{ParallelCommands, SystemParam},
};
use bevy_tasks::ComputeTaskPool;
use bevy_time::Time;
use bevy_utils::Parallel;

use crate::{
    BroadcastStateTreeEvent, EntityStateTreeEvent, InstanceStatus, StateTreeConfiguration,
    StateTreeDefinition, StateTreeEventRef, StateTreeInstance, StateTreeLinkError,
    activity::{
        BevyActivitySink, StateTreeActivityInbox, StateTreeActivityIngress,
        StateTreeActivityRegistry,
    },
    definition::{CompiledDefinition, LinkedDefinition},
    effect::{BevyEffectSink, StateTreeEffectRegistry},
    event::{QueuedStateTreeEvent, StateTreeAppEventRegistry},
    instance::{PendingStateTree, TimerToken, TimerUpdate},
    scxml::ScxmlLoader,
};

#[cfg(feature = "persistence")]
pub(crate) mod persistence;

/// Ordered phases installed into the state-tree schedule.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, SystemSet)]
pub enum StateTreeSet {
    /// Rebuilds execution after definition or configuration replacement.
    /// Hosts must supply current configuration before this phase.
    Reconstruct,
    /// Delivers timers whose deadlines have elapsed.
    ProcessTimers,
    /// Produces state-tree events that must be delivered in this schedule run.
    QueueEvents,
    /// Processes terminal activity outcomes as independent macrosteps.
    ProcessActivityOutcomes,
    /// Initializes instances whose definition assets are available.
    Initialize,
    /// Delivers queued events and runs macrosteps to stability.
    ProcessEvents,
    /// Handles typed activity lifecycle messages.
    HandleActivities,
    /// Consumes typed effect messages emitted by state-tree actions.
    HandleEffects,
}

/// Application-wide state-tree runtime limits.
#[derive(Resource)]
pub struct StateTreeSettings {
    max_microsteps: NonZeroUsize,
}

impl StateTreeSettings {
    /// Creates settings with the maximum transition-set microsteps per macrostep.
    ///
    /// An external-event transition set counts as the first microstep. Initial
    /// configuration entry does not count, but subsequent eventless transition
    /// sets do.
    pub fn new(max_microsteps: NonZeroUsize) -> Self {
        Self { max_microsteps }
    }

    /// Returns the maximum transition-set microsteps per macrostep.
    pub fn max_microsteps(&self) -> NonZeroUsize {
        self.max_microsteps
    }
}

impl Default for StateTreeSettings {
    fn default() -> Self {
        Self::new(NonZeroUsize::new(64).expect("64 is non-zero"))
    }
}

/// Installs the application-wide state-tree runtime into a Bevy schedule.
///
/// The plugin processes each instance's events sequentially but processes
/// separate instances in parallel. Event-producing systems should run in
/// [`StateTreeSet::QueueEvents`] for same-schedule delivery. Events written
/// later remain queued for the next run.
pub struct StateTreePlugin {
    schedule: InternedScheduleLabel,
    configuration: Vec<fn(&mut crate::StateTreeConfig<'_>)>,
}

impl StateTreePlugin {
    /// Creates the runtime that processes events in `schedule`.
    pub fn new(schedule: impl ScheduleLabel) -> Self {
        Self {
            schedule: schedule.intern(),
            configuration: Vec::new(),
        }
    }

    /// Adds an application protocol configuration function.
    ///
    /// Functions run in registration order during plugin installation, before
    /// the protocol is frozen. They may compose other configuration functions.
    pub fn configure(mut self, configure: fn(&mut crate::StateTreeConfig<'_>)) -> Self {
        self.configuration.push(configure);
        self
    }
}

impl Plugin for StateTreePlugin {
    fn build(&self, app: &mut App) {
        assert!(
            !app.world().contains_resource::<StateTreeEventIngress>(),
            "StateTreePlugin was added more than once"
        );
        assert!(
            app.world().contains_resource::<Time>(),
            "StateTreePlugin requires Bevy's TimePlugin to be installed first"
        );

        if !app.world().contains_resource::<StateTreeAssetsInstalled>() {
            assert!(
                app.world().contains_resource::<AssetServer>(),
                "StateTreePlugin requires Bevy's AssetPlugin to be installed first"
            );
            if !app
                .world()
                .contains_resource::<Assets<StateTreeDefinition>>()
            {
                app.init_asset::<StateTreeDefinition>();
            }
            app.register_asset_loader(ScxmlLoader);
            app.insert_resource(StateTreeAssetsInstalled);
        }

        app.init_resource::<StateTreeEventIngress>()
            .init_resource::<StateTreeAppEventRegistry>()
            .init_resource::<StateTreeTimers>()
            .init_resource::<LinkedStateTreeDefinitions>()
            .init_resource::<StateTreeEffectRegistry>()
            .init_resource::<StateTreeActivityRegistry>()
            .init_resource::<StateTreeActivityIngress>()
            .init_resource::<StateTreeSettings>()
            .configure_sets(
                self.schedule,
                (
                    StateTreeSet::Reconstruct,
                    StateTreeSet::ProcessTimers,
                    StateTreeSet::QueueEvents,
                    StateTreeSet::ProcessActivityOutcomes,
                    StateTreeSet::Initialize,
                    StateTreeSet::ProcessEvents,
                    StateTreeSet::HandleActivities,
                    StateTreeSet::HandleEffects,
                )
                    .chain(),
            )
            .add_systems(
                self.schedule,
                (
                    reconstruct_changed
                        .in_set(StateTreeSet::Reconstruct)
                        .run_if(
                            resource_changed::<Assets<StateTreeDefinition>>
                                .or_eager(on_message::<AssetEvent<StateTreeDefinition>>)
                                .or_eager(
                                    any_match_filter::<(
                                        Changed<StateTreeConfiguration>,
                                        Allow<Disabled>,
                                    )>,
                                )
                                .or_eager(any_component_removed::<StateTreeConfiguration>),
                        ),
                    process_timers.in_set(StateTreeSet::ProcessTimers),
                    process_activity_outcomes.in_set(StateTreeSet::ProcessActivityOutcomes),
                    initialize_pending.in_set(StateTreeSet::Initialize),
                    process_events.in_set(StateTreeSet::ProcessEvents),
                    finish_teardown.after(StateTreeSet::HandleEffects),
                ),
            );

        crate::spawn_activity::register(app, self.schedule);
        for configure in &self.configuration {
            configure(&mut crate::StateTreeConfig::new(app, self.schedule));
        }
        crate::protocol::install(app);
        #[cfg(feature = "persistence")]
        crate::persistence::ensure_adapter(app);
    }
}

#[derive(Resource)]
struct StateTreeAssetsInstalled;

#[derive(Resource, Default)]
struct LinkedStateTreeDefinitions {
    definitions: HashMap<u64, CachedLinkedDefinition>,
}

enum CachedLinkedDefinition {
    Linked(Weak<LinkedDefinition>),
    Failed(Weak<StateTreeLinkError>),
}

impl LinkedStateTreeDefinitions {
    fn get_or_insert(
        &mut self,
        compiled: Arc<CompiledDefinition>,
        events: &crate::event::StateTreeEventProtocol,
        effects: &StateTreeEffectRegistry,
        activities: &StateTreeActivityRegistry,
    ) -> Result<Arc<LinkedDefinition>, Arc<StateTreeLinkError>> {
        self.definitions.retain(|_, cached| match cached {
            CachedLinkedDefinition::Linked(linked) => linked.strong_count() > 0,
            CachedLinkedDefinition::Failed(error) => error.strong_count() > 0,
        });
        let key = compiled.identity;
        if let Some(cached) = self.definitions.get(&key) {
            match cached {
                CachedLinkedDefinition::Linked(linked) => {
                    if let Some(linked) = linked.upgrade() {
                        return Ok(linked);
                    }
                }
                CachedLinkedDefinition::Failed(error) => {
                    if let Some(error) = error.upgrade() {
                        return Err(error);
                    }
                }
            }
        }
        match LinkedDefinition::new(compiled, events, effects, activities) {
            Ok(linked) => {
                let linked = Arc::new(linked);
                self.definitions
                    .insert(key, CachedLinkedDefinition::Linked(Arc::downgrade(&linked)));
                Ok(linked)
            }
            Err(error) => {
                let error = Arc::new(error);
                self.definitions
                    .insert(key, CachedLinkedDefinition::Failed(Arc::downgrade(&error)));
                Err(error)
            }
        }
    }
}

/// A pending state-tree instance whose definition cannot link to the protocol.
#[derive(Component, Clone, Debug)]
pub struct StateTreeLinkFailure {
    error: Arc<StateTreeLinkError>,
}

impl StateTreeLinkFailure {
    /// Returns the cached protocol-link error.
    pub fn error(&self) -> &StateTreeLinkError {
        &self.error
    }
}

struct EntityEventEntry<E> {
    batch: u64,
    instance: u64,
    event: E,
    broadcasts_before: usize,
}

#[derive(Component)]
pub(crate) struct StateTreeEventInbox {
    events: Mutex<Vec<EntityEventEntry<QueuedStateTreeEvent>>>,
}

impl Default for StateTreeEventInbox {
    fn default() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
        }
    }
}

impl StateTreeEventInbox {
    pub(crate) fn release_storage(&mut self) {
        *self
            .events
            .get_mut()
            .expect("state-tree event inbox poisoned") = Vec::new();
    }
}

#[derive(Resource)]
struct StateTreeEventIngress {
    broadcasts: Mutex<Vec<QueuedStateTreeEvent>>,
    dirty_entities: Mutex<EntityHashSet>,
    batch: AtomicU64,
    published_broadcasts: AtomicUsize,
}

impl Default for StateTreeEventIngress {
    fn default() -> Self {
        Self {
            broadcasts: Mutex::new(Vec::new()),
            dirty_entities: Mutex::new(EntityHashSet::default()),
            batch: AtomicU64::new(0),
            published_broadcasts: AtomicUsize::new(0),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ScheduledTimer {
    deadline: Duration,
    entity: Entity,
    token: TimerToken,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ActiveTimer {
    deadline: Duration,
    entity: Entity,
}

#[derive(Clone, Copy, Debug)]
struct EntityTimerUpdate {
    entity: Entity,
    update: TimerUpdate,
}

#[derive(Resource, Default)]
pub(crate) struct StateTreeTimers {
    heap: BinaryHeap<Reverse<ScheduledTimer>>,
    active: HashMap<TimerToken, ActiveTimer>,
    staged: Parallel<Vec<EntityTimerUpdate>>,
    merged: Vec<EntityTimerUpdate>,
}

impl StateTreeTimers {
    fn apply(&mut self, entity: Entity, update: TimerUpdate) {
        match update {
            TimerUpdate::Arm { token, deadline } => {
                self.active.insert(token, ActiveTimer { deadline, entity });
                self.heap.push(Reverse(ScheduledTimer {
                    deadline,
                    entity,
                    token,
                }));
            }
            TimerUpdate::Cancel(token) => {
                self.active.remove(&token);
            }
        }
    }

    fn apply_staged(&mut self) {
        self.staged.drain_into(&mut self.merged);
        for index in 0..self.merged.len() {
            let update = self.merged[index];
            self.apply(update.entity, update.update);
        }
        self.merged.clear();
        self.compact_if_needed();
    }

    fn compact_if_needed(&mut self) {
        if self.heap.len() > self.active.len().saturating_mul(2).saturating_add(1024) {
            self.heap.retain(|Reverse(timer)| {
                self.active.get(&timer.token)
                    == Some(&ActiveTimer {
                        deadline: timer.deadline,
                        entity: timer.entity,
                    })
            });
        }
    }

    pub(crate) fn cancel_tokens(&mut self, tokens: impl IntoIterator<Item = TimerToken>) {
        for token in tokens {
            self.active.remove(&token);
        }
    }
}

/// Writes one declared entity event to its target state-tree instance.
///
/// Events are queued independently for each instance. Systems that require
/// same-schedule delivery must run in [`StateTreeSet::QueueEvents`]. Concurrent
/// writers to the same entity are ordered by whichever writer acquires that
/// entity's inbox first; configure an explicit Bevy system order when that
/// ordering must be deterministic.
#[derive(SystemParam)]
pub struct EntityStateTreeEventWriter<'w, 's> {
    ingress: Res<'w, StateTreeEventIngress>,
    registry: Res<'w, StateTreeAppEventRegistry>,
    inboxes: Query<'w, 's, (&'static StateTreeInstance, &'static StateTreeEventInbox)>,
}

impl EntityStateTreeEventWriter<'_, '_> {
    /// Queues `event` and returns whether its target has a state-tree instance.
    pub fn write<M>(&mut self, event: M) -> bool
    where
        M: EntityStateTreeEvent,
    {
        self.enqueue(event, None)
    }

    /// Queues an event only for this activity run. Cancellation/reentry invalidates
    /// it, including when an earlier queued event cancels the activity before delivery.
    pub fn write_for_activity<A, M>(
        &mut self,
        token: crate::StateTreeActivityToken<A>,
        event: M,
    ) -> bool
    where
        A: crate::StateTreeActivity,
        M: EntityStateTreeEvent,
    {
        self.enqueue(event, Some(token.erased()))
    }

    fn enqueue<M: EntityStateTreeEvent>(
        &mut self,
        event: M,
        scope: Option<crate::activity::ErasedActivityToken>,
    ) -> bool {
        let protocol = self.registry.protocol();
        let id = protocol
            .registered_event_id(std::any::TypeId::of::<M>())
            .unwrap_or_else(|| {
                panic!(
                    "state-tree event `{}` is not registered",
                    std::any::type_name::<M>()
                )
            });
        let target = event.event_target();
        let Ok((instance, inbox)) = self.inboxes.get(target) else {
            return false;
        };
        if scope
            .is_some_and(|token| token.entity != target || !instance.accepts_activity_token(token))
        {
            return false;
        }
        let instance = instance.runtime_id();
        let batch = self.ingress.batch.load(Ordering::Acquire);
        let mut events = inbox
            .events
            .lock()
            .expect("state-tree event inbox poisoned");
        if events
            .first()
            .is_some_and(|event| event.instance != instance || event.batch != batch)
        {
            events.clear();
        }
        let was_empty = events.is_empty();
        let published = self.ingress.published_broadcasts.load(Ordering::Acquire);
        let broadcasts_before = events
            .last()
            .map_or(published, |event| event.broadcasts_before.max(published));
        let event = match scope {
            Some(scope) => QueuedStateTreeEvent::for_activity(id, event, scope),
            None => QueuedStateTreeEvent::new(id, event),
        };
        events.push(EntityEventEntry {
            batch,
            instance,
            event,
            broadcasts_before,
        });
        drop(events);

        if was_empty {
            self.ingress
                .dirty_entities
                .lock()
                .expect("state-tree dirty entity set poisoned")
                .insert(target);
        }
        true
    }
}

/// Writes one declared event to every initialized state-tree instance.
///
/// The recipients are the instances initialized when
/// [`StateTreeSet::ProcessEvents`] runs, rather than those present when
/// [`Self::write`] is called. Systems that require same-schedule delivery must
/// run in [`StateTreeSet::QueueEvents`].
#[derive(SystemParam)]
pub struct BroadcastStateTreeEventWriter<'w> {
    ingress: Res<'w, StateTreeEventIngress>,
    registry: Res<'w, StateTreeAppEventRegistry>,
}

impl BroadcastStateTreeEventWriter<'_> {
    /// Queues `event` for broadcast.
    pub fn write<M>(&mut self, event: M)
    where
        M: BroadcastStateTreeEvent,
    {
        let mut broadcasts = self
            .ingress
            .broadcasts
            .lock()
            .expect("state-tree broadcast inbox poisoned");
        broadcasts.push(self.registry.protocol().queue(event).unwrap_or_else(|| {
            panic!(
                "state-tree event `{}` is not registered",
                std::any::type_name::<M>()
            )
        }));
        self.ingress
            .published_broadcasts
            .store(broadcasts.len(), Ordering::Release);
    }
}

fn reconstruct_changed(
    mut commands: Commands,
    assets: Res<Assets<StateTreeDefinition>>,
    mut events: MessageReader<AssetEvent<StateTreeDefinition>>,
    mut removed: RemovedComponents<StateTreeConfiguration>,
    changed: Query<Entity, (Changed<StateTreeConfiguration>, Allow<Disabled>)>,
    instances: Query<(Entity, &StateTreeInstance), Allow<Disabled>>,
) {
    let definition_changed = events
        .read()
        .filter(|event| matches!(event, AssetEvent::Modified { .. }))
        .count()
        > 0;
    let mut entities: EntityHashSet = changed.iter().chain(removed.read()).collect();
    if assets.is_changed() || definition_changed {
        entities.extend(
            instances
                .iter()
                .filter(|(_, instance)| {
                    assets
                        .get(instance.definition())
                        .is_some_and(|asset| !instance.uses_definition(asset))
                })
                .map(|(entity, _)| entity),
        );
    }
    for entity in entities {
        let Ok((_, instance)) = instances.get(entity) else {
            continue;
        };
        if instance.status() == &InstanceStatus::Pending {
            continue;
        }
        commands
            .entity(entity)
            .insert(instance.reconstruct(instance.definition().clone()));
    }
}

type RetiringStateTrees<'w, 's> =
    Query<'w, 's, &'static mut StateTreeInstance, (With<PendingStateTree>, Allow<Disabled>)>;

fn finish_teardown(mut instances: RetiringStateTrees<'_, '_>) {
    for mut instance in &mut instances {
        if instance.teardown_pending() {
            instance.finish_teardown();
        }
    }
}

type PendingStateTrees<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static mut StateTreeInstance,
        Option<&'static StateTreeLinkFailure>,
        Option<&'static StateTreeConfiguration>,
    ),
    With<PendingStateTree>,
>;

type StateTreeRegistries<'w> = (
    Res<'w, StateTreeAppEventRegistry>,
    Res<'w, StateTreeEffectRegistry>,
    Res<'w, StateTreeActivityRegistry>,
);

type StateTreeRuntime<'w> = (
    Res<'w, Time>,
    ResMut<'w, StateTreeTimers>,
    Res<'w, StateTreeSettings>,
    Option<Res<'w, crate::StateTreeTraceLog>>,
);

fn initialize_pending(
    commands: ParallelCommands,
    definitions: Res<Assets<StateTreeDefinition>>,
    mut linked_definitions: ResMut<LinkedStateTreeDefinitions>,
    registries: StateTreeRegistries<'_>,
    runtime: StateTreeRuntime<'_>,
    mut instances: PendingStateTrees<'_, '_>,
) {
    let (time, mut timers, settings, traces) = runtime;
    for (entity, mut instance, previous_failure, configuration) in &mut instances {
        // Let every consumer process cancellation before fresh entry actions
        // and activities can run. Execution is absent during this boundary;
        // no outgoing SCXML is allowed to execute.
        if instance.teardown_pending() {
            continue;
        }
        let Some(definition) = definitions.get(instance.definition()) else {
            if previous_failure.is_some() {
                commands.command_scope(|mut commands| {
                    commands.entity(entity).remove::<StateTreeLinkFailure>();
                });
            }
            continue;
        };
        let linked = match linked_definitions.get_or_insert(
            definition.compiled.clone(),
            registries.0.protocol(),
            &registries.1,
            &registries.2,
        ) {
            Ok(linked) => linked,
            Err(error) => {
                if previous_failure.is_none_or(|previous| !Arc::ptr_eq(&previous.error, &error)) {
                    commands.command_scope(|mut commands| {
                        commands
                            .entity(entity)
                            .insert(StateTreeLinkFailure { error });
                    });
                }
                continue;
            }
        };
        instance.set_configuration(configuration);
        instance.set_time(time.elapsed());
        let has_activities = !linked.compiled.activities.is_empty();
        if let Some(traces) = traces.as_deref() {
            traces.record(entity, |trace| {
                instance.initialize_traced(
                    linked,
                    settings.max_microsteps.get(),
                    entity,
                    &mut BevyEffectSink {
                        commands: &commands,
                    },
                    &mut BevyActivitySink {
                        commands: &commands,
                    },
                    trace,
                );
            });
        } else {
            instance.initialize(
                linked,
                settings.max_microsteps.get(),
                entity,
                &mut BevyEffectSink {
                    commands: &commands,
                },
                &mut BevyActivitySink {
                    commands: &commands,
                },
            );
        }
        for update in instance.drain_timer_updates() {
            timers.apply(entity, update);
        }
        commands.command_scope(|mut commands| {
            let mut entity_commands = commands.entity(entity);
            if has_activities {
                entity_commands.insert_if_new(StateTreeActivityInbox::default());
            }
            entity_commands.remove::<(PendingStateTree, StateTreeLinkFailure)>();
        });
    }
}

fn stage_timer_updates(
    instance: &mut StateTreeInstance,
    entity: Entity,
    staged: &Parallel<Vec<EntityTimerUpdate>>,
) {
    staged.scope(|updates| {
        updates.extend(
            instance
                .drain_timer_updates()
                .map(|update| EntityTimerUpdate { entity, update }),
        );
    });
}

type StateTrees<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static mut StateTreeInstance,
        &'static StateTreeEventInbox,
    ),
>;

type TimedStateTrees<'w, 's> = Query<'w, 's, (Entity, &'static mut StateTreeInstance)>;

fn process_timers(
    commands: ParallelCommands,
    time: Res<Time>,
    settings: Res<StateTreeSettings>,
    traces: Option<Res<crate::StateTreeTraceLog>>,
    mut timers: ResMut<StateTreeTimers>,
    mut instances: TimedStateTrees<'_, '_>,
) {
    let now = time.elapsed();
    timers.compact_if_needed();
    let mut due = Vec::new();
    while timers
        .heap
        .peek()
        .is_some_and(|Reverse(timer)| timer.deadline <= now)
    {
        let Reverse(timer) = timers.heap.pop().expect("peeked timer exists");
        let active = ActiveTimer {
            deadline: timer.deadline,
            entity: timer.entity,
        };
        if timers.active.get(&timer.token) != Some(&active) {
            continue;
        }
        timers.active.remove(&timer.token);
        due.push(timer);
    }
    if due.is_empty() {
        return;
    }

    let microstep_limit = settings.max_microsteps.get();
    let parallel = ComputeTaskPool::try_get().is_some();
    {
        let staged = &timers.staged;
        let mut start = 0;
        while start < due.len() {
            let deadline = due[start].deadline;
            let mut end = start + 1;
            while end < due.len() && due[end].deadline == deadline {
                end += 1;
            }
            let mut by_entity = EntityHashMap::<Range<usize>>::default();
            let mut entities = EntityHashSet::default();
            let mut tokens = Vec::with_capacity(end - start);
            for timer in &due[start..end] {
                let index = tokens.len();
                tokens.push(timer.token);
                by_entity
                    .entry(timer.entity)
                    .and_modify(|range| range.end = index + 1)
                    .or_insert(index..index + 1);
                entities.insert(timer.entity);
            }
            let dense = entities.len().saturating_mul(4) >= instances.iter().len();
            let process_instance =
                |(entity, mut instance): (Entity, Mut<'_, StateTreeInstance>)| {
                    let Some(range) = by_entity.get(&entity) else {
                        return;
                    };
                    let changed = {
                        let instance = instance.bypass_change_detection();
                        instance.set_time(now);
                        if let Some(traces) = traces.as_deref() {
                            traces.record(entity, |trace| {
                                instance.process_timers_traced(
                                    &tokens[range.clone()],
                                    microstep_limit,
                                    entity,
                                    &mut BevyEffectSink {
                                        commands: &commands,
                                    },
                                    &mut BevyActivitySink {
                                        commands: &commands,
                                    },
                                    trace,
                                )
                            })
                        } else {
                            instance.process_timers(
                                &tokens[range.clone()],
                                microstep_limit,
                                entity,
                                &mut BevyEffectSink {
                                    commands: &commands,
                                },
                                &mut BevyActivitySink {
                                    commands: &commands,
                                },
                            )
                        }
                    };
                    stage_timer_updates(instance.bypass_change_detection(), entity, staged);
                    if changed {
                        instance.set_changed();
                    }
                };
            if dense && parallel {
                instances.par_iter_mut().for_each(&process_instance);
            } else if dense {
                for instance in &mut instances {
                    process_instance(instance);
                }
            } else if parallel {
                instances
                    .par_iter_many_unique_mut(&entities)
                    .for_each(|result| {
                        if let Ok(instance) = result {
                            process_instance(instance);
                        }
                    });
            } else {
                for instance in instances.iter_many_unique_mut(&entities).matched() {
                    process_instance(instance);
                }
            }
            start = end;
        }
    }
    timers.apply_staged();
}

fn process_activity_outcomes(
    commands: ParallelCommands,
    ingress: Res<StateTreeActivityIngress>,
    time: Res<Time>,
    mut timers: ResMut<StateTreeTimers>,
    settings: Res<StateTreeSettings>,
    traces: Option<Res<crate::StateTreeTraceLog>>,
    mut instances: Query<(Entity, &mut StateTreeInstance, &StateTreeActivityInbox)>,
) {
    if !ingress
        .dirty_hint
        .swap(false, std::sync::atomic::Ordering::AcqRel)
    {
        return;
    }
    let entities = {
        let mut dirty = ingress
            .dirty
            .lock()
            .expect("state-tree activity dirty set poisoned");
        std::mem::take(&mut *dirty)
    };
    if entities.is_empty() {
        return;
    }
    let now = time.elapsed();
    {
        let staged = &timers.staged;
        let process = |(entity, mut instance, inbox): (
            Entity,
            Mut<'_, StateTreeInstance>,
            &StateTreeActivityInbox,
        )| {
            let mut changed = false;
            instance.bypass_change_detection().set_time(now);
            let mut outcomes = inbox
                .outcomes
                .lock()
                .expect("state-tree activity inbox poisoned");
            for outcome in outcomes.drain(..) {
                changed |= if let Some(traces) = traces.as_deref() {
                    traces.record(entity, |trace| {
                        instance
                            .bypass_change_detection()
                            .process_activity_outcome_traced(
                                outcome,
                                settings.max_microsteps.get(),
                                entity,
                                &mut BevyEffectSink {
                                    commands: &commands,
                                },
                                &mut BevyActivitySink {
                                    commands: &commands,
                                },
                                trace,
                            )
                    })
                } else {
                    instance.bypass_change_detection().process_activity_outcome(
                        outcome,
                        settings.max_microsteps.get(),
                        entity,
                        &mut BevyEffectSink {
                            commands: &commands,
                        },
                        &mut BevyActivitySink {
                            commands: &commands,
                        },
                    )
                };
            }
            stage_timer_updates(instance.bypass_change_detection(), entity, staged);
            if changed {
                instance.set_changed();
            }
        };
        if ComputeTaskPool::try_get().is_some() {
            instances
                .par_iter_many_unique_mut(&entities)
                .for_each(|result| {
                    if let Ok(instance) = result {
                        process(instance);
                    }
                });
        } else {
            for instance in instances.iter_many_unique_mut(&entities).matched() {
                process(instance);
            }
        }
    }
    timers.apply_staged();
}

fn process_events(
    commands: ParallelCommands,
    mut ingress: ResMut<StateTreeEventIngress>,
    registry: Res<StateTreeAppEventRegistry>,
    runtime: StateTreeRuntime<'_>,
    mut instances: StateTrees<'_, '_>,
) {
    let (time, mut timers, settings, traces) = runtime;
    let protocol = registry.protocol();
    let ingress = &mut *ingress;
    let broadcasts = ingress
        .broadcasts
        .get_mut()
        .expect("state-tree broadcast inbox poisoned");
    let dirty_entities = ingress
        .dirty_entities
        .get_mut()
        .expect("state-tree dirty entity set poisoned");
    let batch = *ingress.batch.get_mut();
    if broadcasts.is_empty() && dirty_entities.is_empty() {
        return;
    }
    let microstep_limit = settings.max_microsteps.get();
    let now = time.elapsed();
    {
        let staged = &timers.staged;
        let process_instance = |(entity, mut instance, inbox): (
            Entity,
            Mut<'_, StateTreeInstance>,
            &StateTreeEventInbox,
        )| {
            instance.bypass_change_detection().set_time(now);
            let mut entity_events = inbox
                .events
                .lock()
                .expect("state-tree event inbox poisoned");
            if instance.status() == &InstanceStatus::Pending {
                entity_events.clear();
                return;
            }

            let mut broadcast_cursor = 0;

            for event in entity_events.drain(..) {
                if event.instance != instance.runtime_id() || event.batch != batch {
                    continue;
                }
                debug_assert!(event.broadcasts_before <= broadcasts.len());
                while broadcast_cursor < event.broadcasts_before {
                    let broadcast = broadcasts[broadcast_cursor].view(protocol);
                    if instance.accepts_external_event(broadcast) {
                        process_external_event(
                            &mut instance,
                            broadcast,
                            microstep_limit,
                            entity,
                            &commands,
                            traces.as_deref(),
                        );
                    }
                    broadcast_cursor += 1;
                }
                if event
                    .event
                    .activity_scope()
                    .is_some_and(|token| !instance.accepts_activity_token(token))
                {
                    continue;
                }
                process_external_event(
                    &mut instance,
                    event.event.view(protocol),
                    microstep_limit,
                    entity,
                    &commands,
                    traces.as_deref(),
                );
            }

            for event in &broadcasts[broadcast_cursor..] {
                let event = event.view(protocol);
                if instance.accepts_external_event(event) {
                    process_external_event(
                        &mut instance,
                        event,
                        microstep_limit,
                        entity,
                        &commands,
                        traces.as_deref(),
                    );
                }
            }
            stage_timer_updates(instance.bypass_change_detection(), entity, staged);
        };

        let parallel = ComputeTaskPool::try_get().is_some();
        if broadcasts.is_empty() && parallel {
            instances
                .par_iter_many_unique_mut(&*dirty_entities)
                .for_each(|result| {
                    if let Ok(instance) = result {
                        process_instance(instance);
                    }
                });
        } else if broadcasts.is_empty() {
            for instance in instances.iter_many_unique_mut(&*dirty_entities).matched() {
                process_instance(instance);
            }
        } else if parallel {
            instances.par_iter_mut().for_each(&process_instance);
        } else {
            for instance in &mut instances {
                process_instance(instance);
            }
        }
    }
    broadcasts.clear();
    dirty_entities.clear();
    *ingress.batch.get_mut() = batch.wrapping_add(1);
    *ingress.published_broadcasts.get_mut() = 0;
    timers.apply_staged();
}

fn process_external_event(
    instance: &mut Mut<'_, StateTreeInstance>,
    event: StateTreeEventRef<'_>,
    microstep_limit: usize,
    entity: Entity,
    commands: &ParallelCommands<'_, '_>,
    traces: Option<&crate::StateTreeTraceLog>,
) {
    let changed = if let Some(traces) = traces {
        traces.record(entity, |trace| {
            instance.bypass_change_detection().process_event_traced(
                event,
                microstep_limit,
                entity,
                &mut BevyEffectSink { commands },
                &mut BevyActivitySink { commands },
                trace,
            )
        })
    } else {
        instance.bypass_change_detection().process_event(
            event,
            microstep_limit,
            entity,
            &mut BevyEffectSink { commands },
            &mut BevyActivitySink { commands },
        )
    };
    if changed {
        instance.set_changed();
    }
}

#[cfg(test)]
mod tests {
    use bevy_app::{App, TaskPoolPlugin, Update};
    use bevy_asset::{AssetPlugin, AssetServer, Assets};
    use bevy_ecs::{
        event::{EntityEvent, Event},
        message::{Message, MessageReader},
        prelude::{Changed, Entity},
        schedule::ScheduleBuildSettings,
        system::{In, RunSystemOnce},
    };
    use bevy_time::TimePlugin;

    use super::*;
    use crate::{
        InstanceStatus, StateTreeActivity, StateTreeActivityDefinition, StateTreeActivityLifecycle,
        StateTreeActivityOutcomeWriter, StateTreeBuilder, StateTreeEvent, StateTreeTransition,
    };

    fn configure_simulation(config: &mut crate::StateTreeConfig<'_>) {
        config
            .event::<Advance>()
            .event::<Reset>()
            .event::<ResetAll>()
            .event::<AdvanceAll>()
            .activity::<FindPath, _>("find_path", |_context, _arguments| Ok(FindPath))
            .effect::<EffectPublished, _>("publish", |context, arguments| {
                Ok(EffectPublished {
                    entity: context.entity(),
                    amount: arguments.take("amount")?,
                })
            });
    }

    struct FindPath;

    impl StateTreeActivity for FindPath {
        type Output = i64;
        type Error = String;

        fn output_data(
            output: &Self::Output,
        ) -> Result<Option<crate::LuauValue>, crate::StateTreeEventDataError> {
            if *output == -1 {
                return Err(crate::StateTreeEventDataError::new(
                    "path projection failed",
                ));
            }
            Ok(Some(crate::LuauValue::Integer(*output)))
        }

        fn error_data(
            error: &Self::Error,
        ) -> Result<Option<crate::LuauValue>, crate::StateTreeEventDataError> {
            Ok(Some(crate::LuauValue::String(error.clone())))
        }
    }

    #[derive(Resource, Default)]
    struct ActivityMessages {
        started: Vec<crate::StateTreeActivityToken<FindPath>>,
        cancelled: Vec<crate::StateTreeActivityToken<FindPath>>,
    }

    fn collect_activities(
        mut messages: MessageReader<StateTreeActivityLifecycle<FindPath>>,
        mut recorded: ResMut<ActivityMessages>,
    ) {
        for message in messages.read() {
            match message {
                StateTreeActivityLifecycle::Started { token, .. } => recorded.started.push(*token),
                StateTreeActivityLifecycle::Cancelled { token } => recorded.cancelled.push(*token),
            }
        }
    }

    fn finish_path(
        In(token): In<crate::StateTreeActivityToken<FindPath>>,
        mut writer: StateTreeActivityOutcomeWriter<FindPath>,
    ) -> bool {
        writer.complete(token, 7)
    }

    fn fail_path(
        In(token): In<crate::StateTreeActivityToken<FindPath>>,
        mut writer: StateTreeActivityOutcomeWriter<FindPath>,
    ) -> bool {
        writer.fail(token, "blocked".to_owned())
    }

    fn break_path_projection(
        In(token): In<crate::StateTreeActivityToken<FindPath>>,
        mut writer: StateTreeActivityOutcomeWriter<FindPath>,
    ) -> bool {
        writer.complete(token, -1)
    }

    fn activity_definition() -> StateTreeDefinition {
        let mut builder = StateTreeBuilder::new("activity");
        builder
            .luau_datamodel([crate::StateTreeData::new("result")])
            .unwrap();
        let root = builder.compound_root("root").unwrap();
        let active = builder.atomic("active", root).unwrap();
        let done = builder.final_state("done", root).unwrap();
        builder.initial(root, active).unwrap();
        builder
            .add_activity(
                active,
                StateTreeActivityDefinition::new("path", "find_path"),
            )
            .unwrap();
        builder
            .add_transition(
                active,
                StateTreeTransition::on("done.activity.path")
                    .condition("_event.data == 7")
                    .target(done),
            )
            .unwrap();
        builder.build().unwrap()
    }

    fn activity_precedence_definition() -> StateTreeDefinition {
        let mut builder = StateTreeBuilder::new("activity-precedence");
        let root = builder.compound_root("root").unwrap();
        let active = builder.atomic("active", root).unwrap();
        let activity_won = builder.final_state("activity-won", root).unwrap();
        let event_won = builder.final_state("event-won", root).unwrap();
        builder.initial(root, active).unwrap();
        builder
            .add_activity(
                active,
                StateTreeActivityDefinition::new("path", "find_path"),
            )
            .unwrap();
        builder
            .add_transition(
                active,
                StateTreeTransition::on("done.activity.path").target(activity_won),
            )
            .unwrap();
        builder
            .add_transition(active, StateTreeTransition::on("advance").target(event_won))
            .unwrap();
        builder.build().unwrap()
    }

    fn failing_activity_definition() -> StateTreeDefinition {
        let mut builder = StateTreeBuilder::new("activity-failure");
        let root = builder.compound_root("root").unwrap();
        let active = builder.atomic("active", root).unwrap();
        let failed = builder.final_state("failed", root).unwrap();
        builder.initial(root, active).unwrap();
        builder
            .add_activity(
                active,
                StateTreeActivityDefinition::new("path", "find_path"),
            )
            .unwrap();
        builder
            .add_transition(
                active,
                StateTreeTransition::on("error.activity.path").target(failed),
            )
            .unwrap();
        builder.build().unwrap()
    }

    fn timed_activity_definition() -> StateTreeDefinition {
        let mut builder = StateTreeBuilder::new("timed-activity");
        let root = builder.compound_root("root").unwrap();
        let active = builder.atomic("active", root).unwrap();
        let timed = builder.atomic("timed", root).unwrap();
        let elapsed = builder.atomic("elapsed", root).unwrap();
        let wrong = builder.atomic("wrong", root).unwrap();
        builder.initial(root, active).unwrap();
        builder
            .add_activity(
                active,
                StateTreeActivityDefinition::new("path", "find_path"),
            )
            .unwrap();
        builder
            .add_transition(
                active,
                StateTreeTransition::after(Duration::from_secs(10)).target(wrong),
            )
            .unwrap();
        builder
            .add_transition(
                active,
                StateTreeTransition::on("done.activity.path").target(timed),
            )
            .unwrap();
        builder
            .add_transition(
                timed,
                StateTreeTransition::after(Duration::from_secs(1)).target(elapsed),
            )
            .unwrap();
        builder.build().unwrap()
    }

    #[derive(EntityEvent, crate::StateTreeEvent)]
    #[state_tree(name = "advance", data = project_advance)]
    struct Advance {
        entity: Entity,
        steps: u32,
    }

    #[derive(EntityEvent, crate::StateTreeEvent)]
    #[state_tree(name = "unregistered")]
    struct Unregistered {
        entity: Entity,
    }

    #[derive(EntityEvent, crate::StateTreeEvent)]
    #[state_tree(name = "reset")]
    struct Reset {
        entity: Entity,
    }

    #[derive(Event, crate::StateTreeEvent)]
    #[state_tree(name = "reset.all", broadcast)]
    struct ResetAll {
        reason: u32,
    }

    #[derive(Event, crate::StateTreeEvent)]
    #[state_tree(name = "advance.all", broadcast)]
    struct AdvanceAll;

    fn project_advance(
        event: &Advance,
    ) -> Result<crate::LuauValue, crate::StateTreeEventDataError> {
        Ok(crate::LuauValue::Record(
            [(
                "steps".into(),
                crate::LuauValue::Integer(event.steps.into()),
            )]
            .into(),
        ))
    }

    #[derive(Resource, Default)]
    struct ChangedInstances(usize);

    #[derive(Message)]
    struct EffectPublished {
        entity: Entity,
        amount: i64,
    }

    #[derive(Resource, Default)]
    struct PublishedEffects(Vec<(Entity, i64)>);

    fn collect_effects(
        mut messages: MessageReader<EffectPublished>,
        mut published: ResMut<PublishedEffects>,
    ) {
        published.0.extend(
            messages
                .read()
                .map(|message| (message.entity, message.amount)),
        );
    }

    fn definition() -> StateTreeDefinition {
        let mut builder = StateTreeBuilder::new("messages");
        let root = builder.compound_root("root").unwrap();
        let idle = builder.atomic("idle", root).unwrap();
        let active = builder.atomic("active", root).unwrap();
        builder.initial(root, idle).unwrap();
        builder
            .add_transition(idle, StateTreeTransition::on("advance").target(active))
            .unwrap();
        builder
            .add_transition(idle, StateTreeTransition::on("advance.all").target(active))
            .unwrap();
        builder
            .add_transition(active, StateTreeTransition::on("reset").target(idle))
            .unwrap();
        builder
            .add_transition(active, StateTreeTransition::on("reset.all").target(idle))
            .unwrap();
        builder.build().unwrap()
    }

    fn timeout_definition() -> StateTreeDefinition {
        let mut builder = StateTreeBuilder::new("timeout");
        let root = builder.compound_root("root").unwrap();
        let waiting = builder.atomic("waiting", root).unwrap();
        let elapsed = builder.atomic("elapsed", root).unwrap();
        builder.initial(root, waiting).unwrap();
        builder
            .add_transition(
                waiting,
                StateTreeTransition::after(Duration::from_secs(1)).target(elapsed),
            )
            .unwrap();
        builder.build().unwrap()
    }

    fn uninterested_definition() -> StateTreeDefinition {
        let mut builder = StateTreeBuilder::new("uninterested");
        let root = builder.compound_root("root").unwrap();
        let idle = builder.atomic("idle", root).unwrap();
        builder.initial(root, idle).unwrap();
        builder.build().unwrap()
    }

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins((
            TaskPoolPlugin::default(),
            TimePlugin,
            AssetPlugin {
                file_path: concat!(env!("CARGO_MANIFEST_DIR"), "/tests/assets").into(),
                watch_for_changes_override: Some(false),
                use_asset_processor_override: Some(false),
                ..Default::default()
            },
            StateTreePlugin::new(Update).configure(configure_simulation),
        ));
        app
    }

    fn activity_app() -> App {
        let mut app = app();
        app.init_resource::<ActivityMessages>().add_systems(
            Update,
            collect_activities.in_set(StateTreeSet::HandleActivities),
        );
        app
    }

    #[test]
    fn typed_activities_start_complete_and_ignore_duplicate_outcomes() {
        let mut app = activity_app();
        let handle = app
            .world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .add(activity_definition());
        let entity = app.world_mut().spawn(StateTreeInstance::new(handle)).id();
        app.update();
        let token = app.world().resource::<ActivityMessages>().started[0];
        assert_eq!(token.entity(), entity);
        assert!(
            app.world_mut()
                .run_system_once_with(finish_path, token)
                .unwrap()
        );
        assert!(
            !app.world_mut()
                .run_system_once_with(finish_path, token)
                .unwrap()
        );
        app.update();
        assert_eq!(active_leaf(&app, entity), "done");
        assert!(
            !app.world_mut()
                .run_system_once_with(finish_path, token)
                .unwrap()
        );
        app.update();
        assert_eq!(active_leaf(&app, entity), "done");
    }

    #[test]
    fn activity_outcome_applies_timer_cancellations_and_arms_from_current_time() {
        let mut app = activity_app();
        let handle = app
            .world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .add(timed_activity_definition());
        let entity = app.world_mut().spawn(StateTreeInstance::new(handle)).id();
        app.update();
        advance_clock(&mut app, Duration::from_secs(5));
        let token = app.world().resource::<ActivityMessages>().started[0];
        assert!(
            app.world_mut()
                .run_system_once_with(finish_path, token)
                .unwrap()
        );
        app.update();

        assert_eq!(active_leaf(&app, entity), "timed");
        assert_eq!(app.world().resource::<StateTreeTimers>().active.len(), 1);
        advance_clock(&mut app, Duration::from_millis(999));
        assert_eq!(active_leaf(&app, entity), "timed");
        advance_clock(&mut app, Duration::from_millis(1));
        assert_eq!(active_leaf(&app, entity), "elapsed");
    }

    #[test]
    fn activity_outcomes_precede_external_events_in_the_same_schedule() {
        let mut app = activity_app();
        let handle = app
            .world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .add(activity_precedence_definition());
        let entity = app.world_mut().spawn(StateTreeInstance::new(handle)).id();
        app.update();
        let token = app.world().resource::<ActivityMessages>().started[0];
        assert!(
            app.world_mut()
                .run_system_once_with(finish_path, token)
                .unwrap()
        );
        queue_advance(&mut app, entity);
        app.update();
        assert_eq!(active_leaf(&app, entity), "activity-won");
    }

    #[test]
    fn activity_failure_uses_its_separate_error_event() {
        let mut app = activity_app();
        let handle = app
            .world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .add(failing_activity_definition());
        let entity = app.world_mut().spawn(StateTreeInstance::new(handle)).id();
        app.update();
        let token = app.world().resource::<ActivityMessages>().started[0];
        assert!(
            app.world_mut()
                .run_system_once_with(fail_path, token)
                .unwrap()
        );
        app.update();
        assert_eq!(active_leaf(&app, entity), "failed");
    }

    #[test]
    fn activity_projection_errors_fault_only_the_target_instance() {
        let mut app = activity_app();
        let handle = app
            .world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .add(activity_definition());
        let faulted = app
            .world_mut()
            .spawn(StateTreeInstance::new(handle.clone()))
            .id();
        let running = app.world_mut().spawn(StateTreeInstance::new(handle)).id();
        app.update();
        let token = app
            .world()
            .resource::<ActivityMessages>()
            .started
            .iter()
            .copied()
            .find(|token| token.entity() == faulted)
            .unwrap();
        assert!(
            app.world_mut()
                .run_system_once_with(break_path_projection, token)
                .unwrap()
        );
        app.update();
        assert!(matches!(
            app.world()
                .get::<StateTreeInstance>(faulted)
                .unwrap()
                .status(),
            InstanceStatus::Faulted(crate::RuntimeFault::ActivityDataProjection { .. })
        ));
        assert_eq!(
            app.world()
                .get::<StateTreeInstance>(running)
                .unwrap()
                .status(),
            &InstanceStatus::Running
        );
    }

    #[test]
    fn removing_or_replacing_an_instance_cancels_and_invalidates_activity_generations() {
        let mut app = activity_app();
        let handle = app
            .world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .add(activity_definition());
        let entity = app
            .world_mut()
            .spawn(StateTreeInstance::new(handle.clone()))
            .id();
        app.update();
        let token = app.world().resource::<ActivityMessages>().started[0];
        app.world_mut()
            .entity_mut(entity)
            .remove::<StateTreeInstance>();
        app.update();
        assert_eq!(
            app.world().resource::<ActivityMessages>().cancelled,
            [token]
        );
        app.world_mut()
            .entity_mut(entity)
            .insert(StateTreeInstance::new(handle));
        app.update();
        let replacement = *app
            .world()
            .resource::<ActivityMessages>()
            .started
            .last()
            .unwrap();
        assert_ne!(token, replacement);
        assert!(
            !app.world_mut()
                .run_system_once_with(finish_path, token)
                .unwrap()
        );
        app.update();
        assert_eq!(active_leaf(&app, entity), "active");
    }

    #[test]
    fn taking_and_reinserting_the_same_instance_invalidates_old_activity_tokens() {
        let mut app = activity_app();
        let handle = app
            .world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .add(activity_definition());
        let entity = app.world_mut().spawn(StateTreeInstance::new(handle)).id();
        app.update();
        let token = app.world().resource::<ActivityMessages>().started[0];

        let instance = app
            .world_mut()
            .entity_mut(entity)
            .take::<StateTreeInstance>()
            .unwrap();
        app.world_mut().entity_mut(entity).insert(instance);

        assert!(
            !app.world_mut()
                .run_system_once_with(break_path_projection, token)
                .unwrap()
        );
        app.update();
        app.update();
        assert_eq!(active_leaf(&app, entity), "active");
    }

    #[test]
    fn removing_an_instance_clears_queued_activity_outcomes_and_dirty_ingress() {
        let mut app = activity_app();
        let handle = app
            .world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .add(activity_definition());
        let entity = app.world_mut().spawn(StateTreeInstance::new(handle)).id();
        app.update();
        let token = app.world().resource::<ActivityMessages>().started[0];
        assert!(
            app.world_mut()
                .run_system_once_with(finish_path, token)
                .unwrap()
        );

        let instance = app
            .world_mut()
            .entity_mut(entity)
            .take::<StateTreeInstance>()
            .unwrap();
        assert!(
            app.world()
                .get::<StateTreeActivityInbox>(entity)
                .unwrap()
                .outcomes
                .lock()
                .unwrap()
                .is_empty()
        );
        assert!(
            !app.world()
                .resource::<StateTreeActivityIngress>()
                .dirty
                .lock()
                .unwrap()
                .contains(&entity)
        );
        app.world_mut().entity_mut(entity).insert(instance);
        app.update();
        app.update();
        assert_eq!(active_leaf(&app, entity), "active");
    }

    #[test]
    fn configuration_is_used_to_validate_activities() {
        fn configure_missing_activity(_config: &mut crate::StateTreeConfig<'_>) {}
        let mut app = App::new();
        app.add_plugins((
            TimePlugin,
            AssetPlugin::default(),
            StateTreePlugin::new(Update).configure(configure_missing_activity),
        ));
        let handle = app
            .world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .add(activity_definition());
        let entity = app.world_mut().spawn(StateTreeInstance::new(handle)).id();
        app.update();
        assert!(matches!(
            app.world()
                .get::<StateTreeLinkFailure>(entity)
                .unwrap()
                .error()
                .issues()[0],
            crate::StateTreeLinkIssue::MissingActivity { .. }
        ));
    }

    fn update_until(app: &mut App, expectation: &str, predicate: impl Fn(&App) -> bool) {
        for _ in 0..1_000 {
            app.update();
            if predicate(app) {
                return;
            }
            std::thread::yield_now();
        }
        panic!("timed out waiting for {expectation}");
    }

    #[test]
    fn typed_effects_publish_concrete_messages_before_handle_effects() {
        let mut app = app();
        app.init_resource::<PublishedEffects>()
            .add_systems(Update, collect_effects.in_set(StateTreeSet::HandleEffects));
        let handle = app.world().resource::<AssetServer>().load("effect.scxml");
        let entity = app.world_mut().spawn(StateTreeInstance::new(handle)).id();

        update_until(&mut app, "the entry effect", |app| {
            !app.world().resource::<PublishedEffects>().0.is_empty()
        });

        assert_eq!(app.world().resource::<PublishedEffects>().0, [(entity, 7)]);
    }

    #[test]
    fn invalid_links_remain_pending_until_the_asset_is_replaced() {
        let mut builder = StateTreeBuilder::new("invalid-link");
        let root = builder.compound_root("root").unwrap();
        let idle = builder.atomic("idle", root).unwrap();
        let done = builder.atomic("done", root).unwrap();
        builder.initial(root, idle).unwrap();
        builder
            .add_transition(idle, StateTreeTransition::on("adavnce").target(done))
            .unwrap();
        let mut app = app();
        let handle = app
            .world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .add(builder.build().unwrap());
        let entity = app
            .world_mut()
            .spawn(StateTreeInstance::new(handle.clone()))
            .id();

        app.update();

        let entity_ref = app.world().entity(entity);
        assert_eq!(
            entity_ref.get::<StateTreeInstance>().unwrap().status(),
            &InstanceStatus::Pending
        );
        assert!(matches!(
            &entity_ref
                .get::<StateTreeLinkFailure>()
                .unwrap()
                .error()
                .issues()[0],
            crate::StateTreeLinkIssue::UnmatchedEventDescriptor { descriptor, .. }
                if descriptor.as_ref() == "adavnce"
        ));

        app.world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .insert(handle.id(), definition())
            .unwrap();
        app.update();

        let entity_ref = app.world().entity(entity);
        assert_eq!(
            entity_ref.get::<StateTreeInstance>().unwrap().status(),
            &InstanceStatus::Running
        );
        assert!(entity_ref.get::<StateTreeLinkFailure>().is_none());
    }

    #[test]
    fn configuration_is_used_to_validate_effects() {
        fn configure_missing_effect(_config: &mut crate::StateTreeConfig<'_>) {}
        let mut app = App::new();
        app.add_plugins((
            TimePlugin,
            AssetPlugin {
                file_path: concat!(env!("CARGO_MANIFEST_DIR"), "/tests/assets").into(),
                watch_for_changes_override: Some(false),
                use_asset_processor_override: Some(false),
                ..Default::default()
            },
            StateTreePlugin::new(Update).configure(configure_missing_effect),
        ));
        let handle = app.world().resource::<AssetServer>().load("effect.scxml");
        let entity = app.world_mut().spawn(StateTreeInstance::new(handle)).id();

        update_until(&mut app, "the effect link failure", |app| {
            app.world().get::<StateTreeLinkFailure>(entity).is_some()
        });

        assert!(matches!(
            &app.world()
                .entity(entity)
                .get::<StateTreeLinkFailure>()
                .unwrap()
                .error()
                .issues()[0],
            crate::StateTreeLinkIssue::MissingEffect { effect, .. }
                if effect.as_ref() == "publish"
        ));
    }

    #[test]
    fn plugin_preserves_an_existing_definition_asset_store() {
        let mut app = App::new();
        app.add_plugins((TimePlugin, AssetPlugin::default()))
            .init_asset::<StateTreeDefinition>();
        let handle = app
            .world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .add(definition());

        app.add_plugins(StateTreePlugin::new(Update).configure(configure_simulation));
        assert!(
            app.world()
                .resource::<Assets<StateTreeDefinition>>()
                .contains(handle.id())
        );
    }

    #[test]
    fn derived_events_project_opt_in_luau_data() {
        let event = Advance {
            entity: Entity::PLACEHOLDER,
            steps: 9,
        };

        assert_eq!(
            event.event_data().unwrap(),
            Some(crate::LuauValue::Record(
                [("steps".into(), crate::LuauValue::Integer(9))].into(),
            ))
        );
    }

    fn write_advance(In(event): In<Advance>, mut writer: EntityStateTreeEventWriter) -> bool {
        writer.write(event)
    }

    fn write_reset(In(event): In<Reset>, mut writer: EntityStateTreeEventWriter) -> bool {
        writer.write(event)
    }

    fn write_reset_all(mut writer: BroadcastStateTreeEventWriter) {
        writer.write(ResetAll { reason: 42 });
    }

    fn write_advance_all(mut writer: BroadcastStateTreeEventWriter) {
        writer.write(AdvanceAll);
    }

    fn record_changed_instances(
        instances: Query<(), Changed<StateTreeInstance>>,
        mut changed: ResMut<ChangedInstances>,
    ) {
        changed.0 = instances.iter().count();
    }

    fn queue_advance(app: &mut App, entity: Entity) {
        assert!(
            app.world_mut()
                .run_system_once_with(write_advance, Advance { entity, steps: 1 })
                .unwrap()
        );
    }

    fn queue_reset(app: &mut App, entity: Entity) {
        assert!(
            app.world_mut()
                .run_system_once_with(write_reset, Reset { entity })
                .unwrap()
        );
    }

    fn queue_reset_all(app: &mut App) {
        app.world_mut().run_system_once(write_reset_all).unwrap();
    }

    fn queue_advance_all(app: &mut App) {
        app.world_mut().run_system_once(write_advance_all).unwrap();
    }

    fn active_leaf(app: &App, entity: Entity) -> String {
        app.world()
            .get::<StateTreeInstance>(entity)
            .unwrap()
            .active_state_names()
            .last()
            .unwrap()
            .to_owned()
    }

    fn advance_clock(app: &mut App, duration: Duration) {
        app.world_mut().resource_mut::<Time>().advance_by(duration);
        app.world_mut().run_schedule(Update);
    }

    #[test]
    fn bevy_clock_delivers_due_after_transitions() {
        let mut app = app();
        let handle = app
            .world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .add(timeout_definition());
        let entity = app.world_mut().spawn(StateTreeInstance::new(handle)).id();
        app.update();

        advance_clock(&mut app, Duration::from_millis(999));
        assert_eq!(active_leaf(&app, entity), "waiting");
        advance_clock(&mut app, Duration::from_millis(1));
        assert_eq!(active_leaf(&app, entity), "elapsed");
    }

    #[test]
    fn due_timeout_precedes_an_external_event_in_the_same_schedule_run() {
        let mut builder = StateTreeBuilder::new("timeout-order");
        let root = builder.compound_root("root").unwrap();
        let waiting = builder.atomic("waiting", root).unwrap();
        let timed_out = builder.atomic("timed-out", root).unwrap();
        let event_won = builder.atomic("event-won", root).unwrap();
        builder.initial(root, waiting).unwrap();
        builder
            .add_transition(
                waiting,
                StateTreeTransition::after(Duration::from_secs(1)).target(timed_out),
            )
            .unwrap();
        builder
            .add_transition(
                waiting,
                StateTreeTransition::on("advance").target(event_won),
            )
            .unwrap();
        let mut app = app();
        let handle = app
            .world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .add(builder.build().unwrap());
        let entity = app.world_mut().spawn(StateTreeInstance::new(handle)).id();
        app.update();

        app.world_mut()
            .resource_mut::<Time>()
            .advance_by(Duration::from_secs(1));
        queue_advance(&mut app, entity);
        app.world_mut().run_schedule(Update);

        assert_eq!(active_leaf(&app, entity), "timed-out");
    }

    #[test]
    fn removing_or_replacing_an_instance_invalidates_its_stale_timers() {
        let mut app = app();
        let handle = app
            .world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .add(timeout_definition());
        let removed = app
            .world_mut()
            .spawn(StateTreeInstance::new(handle.clone()))
            .id();
        let replaced = app
            .world_mut()
            .spawn(StateTreeInstance::new(handle.clone()))
            .id();
        app.update();
        assert_eq!(app.world().resource::<StateTreeTimers>().active.len(), 2);

        app.world_mut()
            .entity_mut(removed)
            .remove::<StateTreeInstance>();
        assert_eq!(app.world().resource::<StateTreeTimers>().active.len(), 1);
        app.world_mut()
            .resource_mut::<Time>()
            .advance_by(Duration::from_millis(500));
        app.world_mut()
            .entity_mut(replaced)
            .insert(StateTreeInstance::new(handle));
        app.world_mut().run_schedule(Update);

        advance_clock(&mut app, Duration::from_millis(500));

        assert!(app.world().get::<StateTreeInstance>(removed).is_none());
        assert_eq!(active_leaf(&app, replaced), "waiting");

        advance_clock(&mut app, Duration::from_millis(500));

        assert_eq!(active_leaf(&app, replaced), "elapsed");
        assert!(app.world().resource::<StateTreeTimers>().active.is_empty());
    }

    #[test]
    fn entity_events_only_advance_the_target_instance() {
        let mut app = app();
        let handle = app
            .world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .add(definition());
        let first = app
            .world_mut()
            .spawn(StateTreeInstance::new(handle.clone()))
            .id();
        let second = app.world_mut().spawn(StateTreeInstance::new(handle)).id();
        app.update();

        queue_advance(&mut app, first);
        app.update();

        assert_eq!(active_leaf(&app, first), "active");
        assert_eq!(active_leaf(&app, second), "idle");
    }

    #[test]
    #[should_panic(expected = "is not registered")]
    fn unregistered_entity_events_panic_even_when_the_target_is_absent() {
        let mut app = app();

        app.world_mut()
            .run_system_once(|mut events: EntityStateTreeEventWriter| {
                events.write(Unregistered {
                    entity: Entity::PLACEHOLDER,
                });
            })
            .unwrap();
    }

    #[test]
    fn entity_event_payload_is_retained_in_the_inbox() {
        let mut app = app();
        let handle = app
            .world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .add(definition());
        let entity = app.world_mut().spawn(StateTreeInstance::new(handle)).id();
        app.update();

        assert!(
            app.world_mut()
                .run_system_once_with(write_advance, Advance { entity, steps: 7 })
                .unwrap()
        );

        let inbox = app.world().get::<StateTreeEventInbox>(entity).unwrap();
        let events = inbox.events.lock().unwrap();
        let payload = events[0].event.project::<Advance>().unwrap();
        assert_eq!(payload.steps, 7);
    }

    #[test]
    fn broadcasts_reach_every_initialized_instance() {
        let mut app = app();
        let handle = app
            .world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .add(definition());
        let first = app
            .world_mut()
            .spawn(StateTreeInstance::new(handle.clone()))
            .id();
        let second = app.world_mut().spawn(StateTreeInstance::new(handle)).id();
        app.update();
        queue_advance(&mut app, first);
        queue_advance(&mut app, second);
        app.update();
        assert_eq!(active_leaf(&app, first), "active");
        assert_eq!(active_leaf(&app, second), "active");

        queue_reset_all(&mut app);
        app.update();

        assert_eq!(active_leaf(&app, first), "idle");
        assert_eq!(active_leaf(&app, second), "idle");
    }

    #[test]
    fn broadcasts_skip_definitions_that_do_not_accept_the_event() {
        let mut app = app();
        app.add_plugins(crate::StateTreeTracePlugin);
        let (interested_handle, uninterested_handle) = {
            let mut definitions = app
                .world_mut()
                .resource_mut::<Assets<StateTreeDefinition>>();
            (
                definitions.add(definition()),
                definitions.add(uninterested_definition()),
            )
        };
        let interested = app
            .world_mut()
            .spawn(StateTreeInstance::new(interested_handle))
            .id();
        let uninterested = app
            .world_mut()
            .spawn(StateTreeInstance::new(uninterested_handle))
            .id();
        app.update();

        queue_advance_all(&mut app);
        app.update();

        assert_eq!(active_leaf(&app, interested), "active");
        assert_eq!(active_leaf(&app, uninterested), "idle");
        let traces = app.world().resource::<crate::StateTreeTraceLog>();
        assert!(
            traces
                .get(interested)
                .unwrap()
                .as_str()
                .contains("macrostep external \"advance.all\"")
        );
        assert!(
            !traces
                .get(uninterested)
                .unwrap()
                .as_str()
                .contains("macrostep external \"advance.all\"")
        );
    }

    #[test]
    fn broadcast_event_payload_is_retained_in_ingress() {
        let mut app = app();

        queue_reset_all(&mut app);

        let ingress = app.world().resource::<StateTreeEventIngress>();
        let broadcasts = ingress.broadcasts.lock().unwrap();
        let payload = broadcasts[0].project::<ResetAll>().unwrap();
        assert_eq!(payload.reason, 42);
    }

    #[test]
    fn entity_events_preserve_order_across_rust_types() {
        let mut app = app();
        let handle = app
            .world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .add(definition());
        let entity = app.world_mut().spawn(StateTreeInstance::new(handle)).id();
        app.update();

        queue_advance(&mut app, entity);
        queue_reset(&mut app, entity);
        app.update();
        assert_eq!(active_leaf(&app, entity), "idle");

        queue_reset(&mut app, entity);
        queue_advance(&mut app, entity);
        app.update();
        assert_eq!(active_leaf(&app, entity), "active");
    }

    #[test]
    fn ignored_events_do_not_mark_instances_changed() {
        let mut app = app();
        app.init_resource::<ChangedInstances>().add_systems(
            Update,
            record_changed_instances.after(StateTreeSet::ProcessEvents),
        );
        let handle = app
            .world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .add(definition());
        let entity = app.world_mut().spawn(StateTreeInstance::new(handle)).id();
        app.update();

        queue_reset(&mut app, entity);
        app.update();
        assert_eq!(app.world().resource::<ChangedInstances>().0, 0);

        queue_advance(&mut app, entity);
        app.update();
        assert_eq!(app.world().resource::<ChangedInstances>().0, 1);
    }

    #[test]
    fn broadcasts_interleave_with_each_entity_inbox() {
        let mut app = app();
        let handle = app
            .world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .add(definition());
        let first = app
            .world_mut()
            .spawn(StateTreeInstance::new(handle.clone()))
            .id();
        let second = app.world_mut().spawn(StateTreeInstance::new(handle)).id();
        app.update();

        queue_advance(&mut app, first);
        queue_reset_all(&mut app);
        queue_advance(&mut app, first);
        app.update();

        assert_eq!(active_leaf(&app, first), "active");
        assert_eq!(active_leaf(&app, second), "idle");
    }

    #[test]
    fn initialization_delivery_does_not_require_automatic_deferred_application() {
        let mut app = app();
        app.edit_schedule(Update, |schedule| {
            schedule.set_build_settings(ScheduleBuildSettings {
                auto_insert_apply_deferred: false,
                ..Default::default()
            });
        });
        let handle = app
            .world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .add(definition());
        let first = app
            .world_mut()
            .spawn(StateTreeInstance::new(handle.clone()))
            .id();
        let second = app.world_mut().spawn(StateTreeInstance::new(handle)).id();

        queue_advance(&mut app, first);
        queue_advance_all(&mut app);
        app.update();

        assert_eq!(active_leaf(&app, first), "active");
        assert_eq!(active_leaf(&app, second), "active");
    }

    #[test]
    fn events_are_not_replayed_after_an_instance_remains_pending() {
        let mut app = app();
        let handle = app
            .world()
            .resource::<Assets<StateTreeDefinition>>()
            .reserve_handle();
        let entity = app
            .world_mut()
            .spawn(StateTreeInstance::new(handle.clone()))
            .id();

        queue_advance(&mut app, entity);
        queue_advance_all(&mut app);
        app.update();
        assert_eq!(
            app.world()
                .get::<StateTreeInstance>(entity)
                .unwrap()
                .status(),
            &InstanceStatus::Pending
        );

        app.world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .insert(handle.id(), definition())
            .unwrap();
        app.update();

        assert_eq!(active_leaf(&app, entity), "idle");
    }

    #[test]
    fn replacing_an_instance_does_not_replay_its_old_inbox() {
        let mut app = app();
        let handle = app
            .world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .add(definition());
        let entity = app
            .world_mut()
            .spawn(StateTreeInstance::new(handle.clone()))
            .id();
        app.update();

        queue_advance(&mut app, entity);
        assert!(
            app.world()
                .get::<StateTreeEventInbox>(entity)
                .unwrap()
                .events
                .lock()
                .unwrap()
                .capacity()
                > 0
        );
        app.world_mut()
            .entity_mut(entity)
            .remove::<StateTreeInstance>();
        assert_eq!(
            app.world()
                .get::<StateTreeEventInbox>(entity)
                .unwrap()
                .events
                .lock()
                .unwrap()
                .capacity(),
            0
        );
        app.world_mut()
            .entity_mut(entity)
            .insert(StateTreeInstance::new(handle));
        app.update();

        assert_eq!(active_leaf(&app, entity), "idle");
    }

    #[test]
    fn removing_or_replacing_an_instance_cleans_its_trace_log() {
        let mut app = app();
        app.add_plugins(crate::StateTreeTracePlugin);
        let handle = app
            .world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .add(definition());
        let removed = app
            .world_mut()
            .spawn(StateTreeInstance::new(handle.clone()))
            .id();
        let replaced = app
            .world_mut()
            .spawn(StateTreeInstance::new(handle.clone()))
            .id();
        app.update();

        let traces = app.world().resource::<crate::StateTreeTraceLog>();
        assert!(traces.get(removed).is_some());
        assert!(traces.get(replaced).is_some());

        app.world_mut()
            .entity_mut(removed)
            .remove::<StateTreeInstance>();
        assert!(
            app.world()
                .resource::<crate::StateTreeTraceLog>()
                .get(removed)
                .is_none()
        );

        app.world_mut()
            .entity_mut(replaced)
            .insert(StateTreeInstance::new(handle));
        assert!(
            app.world()
                .resource::<crate::StateTreeTraceLog>()
                .get(replaced)
                .is_none()
        );
        app.update();
        let replacement_log = app
            .world()
            .resource::<crate::StateTreeTraceLog>()
            .get(replaced)
            .unwrap();
        assert_eq!(
            replacement_log
                .as_str()
                .matches("macrostep initialize\n")
                .count(),
            1
        );
    }

    #[test]
    fn an_inbox_detached_for_a_dispatch_does_not_replay_its_old_batch() {
        let mut app = app();
        let handle = app
            .world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .add(definition());
        let entity = app.world_mut().spawn(StateTreeInstance::new(handle)).id();
        app.update();

        queue_reset_all(&mut app);
        queue_advance(&mut app, entity);
        let instance = app
            .world_mut()
            .entity_mut(entity)
            .take::<StateTreeInstance>()
            .unwrap();
        app.update();

        app.world_mut().entity_mut(entity).insert(instance);
        queue_reset_all(&mut app);
        app.update();
        app.update();

        assert_eq!(active_leaf(&app, entity), "idle");
    }

    #[test]
    fn unavailable_assets_remain_pending_without_affecting_ready_instances() {
        let mut app = app();
        let ready_handle = app
            .world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .add(definition());
        let ready = app
            .world_mut()
            .spawn(StateTreeInstance::new(ready_handle))
            .id();
        let pending_handle = app
            .world()
            .resource::<Assets<StateTreeDefinition>>()
            .reserve_handle();
        let pending = app
            .world_mut()
            .spawn(StateTreeInstance::new(pending_handle))
            .id();

        app.update();

        assert_eq!(
            app.world()
                .get::<StateTreeInstance>(ready)
                .unwrap()
                .status(),
            &InstanceStatus::Running
        );
        assert_eq!(
            app.world()
                .get::<StateTreeInstance>(pending)
                .unwrap()
                .status(),
            &InstanceStatus::Pending
        );
    }
}
