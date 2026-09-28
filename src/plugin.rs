use std::{
    marker::PhantomData,
    num::NonZeroUsize,
    sync::{
        Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

use bevy_app::{App, Plugin};
use bevy_asset::{AssetApp, AssetServer, Assets};
use bevy_ecs::{
    change_detection::DetectChangesMut,
    entity::EntityHashSet,
    event::{EntityEvent, Event},
    prelude::{Commands, Component, Entity, Mut, Query, Res, ResMut, Resource, With},
    schedule::{InternedScheduleLabel, IntoScheduleConfigs, ScheduleLabel, SystemSet},
    system::SystemParam,
};
use bevy_tasks::ComputeTaskPool;

use crate::{InstanceStatus, StateTreeDefinition, StateTreeInstance, instance::PendingStateTree};

/// Ordered phases installed into each state-tree domain schedule.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, SystemSet)]
pub enum StateTreeSet {
    /// Produces state-tree events that must be delivered in this schedule run.
    QueueEvents,
    /// Initializes instances whose definition assets are available.
    Initialize,
    /// Delivers queued events and runs macrosteps to stability.
    ProcessEvents,
}

/// Runtime limits for one state-tree domain.
#[derive(Resource)]
pub struct StateTreeSettings<D: Send + Sync + 'static> {
    max_microsteps: NonZeroUsize,
    marker: PhantomData<fn() -> D>,
}

impl<D: Send + Sync + 'static> StateTreeSettings<D> {
    /// Creates settings with the maximum immediate transitions per macrostep.
    pub fn new(max_microsteps: NonZeroUsize) -> Self {
        Self {
            max_microsteps,
            marker: PhantomData,
        }
    }

    /// Returns the maximum immediate transitions per macrostep.
    pub fn max_microsteps(&self) -> NonZeroUsize {
        self.max_microsteps
    }
}

impl<D: Send + Sync + 'static> Default for StateTreeSettings<D> {
    fn default() -> Self {
        Self::new(NonZeroUsize::new(64).expect("64 is non-zero"))
    }
}

/// Installs one independent state-tree domain into a Bevy schedule.
///
/// The plugin processes each instance's events sequentially but processes
/// separate instances in parallel. Event-producing systems should run in
/// [`StateTreeSet::QueueEvents`] for same-schedule delivery. Events written
/// later remain queued for the next run.
pub struct StateTreePlugin<D: Send + Sync + 'static> {
    schedule: InternedScheduleLabel,
    marker: PhantomData<fn() -> D>,
}

impl<D: Send + Sync + 'static> StateTreePlugin<D> {
    /// Creates a domain that processes events in `schedule`.
    pub fn new(schedule: impl ScheduleLabel) -> Self {
        Self {
            schedule: schedule.intern(),
            marker: PhantomData,
        }
    }
}

impl<D: Send + Sync + 'static> Plugin for StateTreePlugin<D> {
    fn build(&self, app: &mut App) {
        assert!(
            !app.world().contains_resource::<DomainSchedule<D>>(),
            "StateTreePlugin was added more than once for this domain"
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
            app.insert_resource(StateTreeAssetsInstalled);
        }

        app.insert_resource(DomainSchedule::<D> {
            schedule: self.schedule,
            marker: PhantomData,
        })
        .init_resource::<StateTreeEventBindings<D>>()
        .init_resource::<StateTreeEventIngress<D>>()
        .init_resource::<StateTreeSettings<D>>()
        .configure_sets(
            self.schedule,
            (
                StateTreeSet::QueueEvents,
                StateTreeSet::Initialize,
                StateTreeSet::ProcessEvents,
            )
                .chain(),
        )
        .add_systems(
            self.schedule,
            (
                initialize_pending::<D>.in_set(StateTreeSet::Initialize),
                process_events::<D>.in_set(StateTreeSet::ProcessEvents),
            ),
        );
    }
}

/// Registers Rust event types as state-tree events.
pub trait StateTreeAppExt {
    /// Registers an event delivered only to the instance on its entity-event target.
    ///
    /// Registration associates `M` with one authored event name. Write `M`
    /// through [`EntityStateTreeEventWriter`]; ordinary Bevy message and
    /// observer APIs do not feed the state-tree queue.
    fn register_entity_state_tree_event<D, M>(
        &mut self,
        authored_name: impl Into<Box<str>>,
    ) -> &mut Self
    where
        D: Send + Sync + 'static,
        M: EntityEvent;

    /// Registers an event delivered to every initialized instance in the domain.
    ///
    /// Registration associates `M` with one authored event name. Write `M`
    /// through [`BroadcastStateTreeEventWriter`]; ordinary Bevy message and
    /// observer APIs do not feed the state-tree queue.
    fn register_broadcast_state_tree_event<D, M>(
        &mut self,
        authored_name: impl Into<Box<str>>,
    ) -> &mut Self
    where
        D: Send + Sync + 'static,
        M: Event;
}

impl StateTreeAppExt for App {
    fn register_entity_state_tree_event<D, M>(
        &mut self,
        authored_name: impl Into<Box<str>>,
    ) -> &mut Self
    where
        D: Send + Sync + 'static,
        M: EntityEvent,
    {
        domain_schedule::<D>(self);
        insert_binding::<D, M>(self, authored_name.into());
        self
    }

    fn register_broadcast_state_tree_event<D, M>(
        &mut self,
        authored_name: impl Into<Box<str>>,
    ) -> &mut Self
    where
        D: Send + Sync + 'static,
        M: Event,
    {
        domain_schedule::<D>(self);
        insert_binding::<D, M>(self, authored_name.into());
        self
    }
}

#[derive(Resource)]
struct StateTreeAssetsInstalled;

#[derive(Resource)]
struct DomainSchedule<D: Send + Sync + 'static> {
    schedule: InternedScheduleLabel,
    marker: PhantomData<fn() -> D>,
}

#[derive(Resource)]
struct EventBinding<D: Send + Sync + 'static, M: Event> {
    id: EventBindingId,
    marker: PhantomData<fn() -> (D, M)>,
}

#[derive(Clone, Copy)]
struct EventBindingId(usize);

#[derive(Resource)]
struct StateTreeEventBindings<D: Send + Sync + 'static> {
    authored_names: Vec<Box<str>>,
    marker: PhantomData<fn() -> D>,
}

impl<D: Send + Sync + 'static> Default for StateTreeEventBindings<D> {
    fn default() -> Self {
        Self {
            authored_names: Vec::new(),
            marker: PhantomData,
        }
    }
}

struct BroadcastEvent {
    binding: EventBindingId,
}

struct EntityEventEntry {
    batch: u64,
    instance: u64,
    binding: EventBindingId,
    broadcasts_before: usize,
}

#[derive(Component)]
pub(crate) struct StateTreeEventInbox<D: Send + Sync + 'static> {
    events: Mutex<Vec<EntityEventEntry>>,
    marker: PhantomData<fn() -> D>,
}

impl<D: Send + Sync + 'static> Default for StateTreeEventInbox<D> {
    fn default() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            marker: PhantomData,
        }
    }
}

impl<D: Send + Sync + 'static> StateTreeEventInbox<D> {
    pub(crate) fn release_storage(&mut self) {
        *self
            .events
            .get_mut()
            .expect("state-tree event inbox poisoned") = Vec::new();
    }
}

#[derive(Resource)]
struct StateTreeEventIngress<D: Send + Sync + 'static> {
    broadcasts: Mutex<Vec<BroadcastEvent>>,
    dirty_entities: Mutex<EntityHashSet>,
    batch: AtomicU64,
    published_broadcasts: AtomicUsize,
    marker: PhantomData<fn() -> D>,
}

impl<D: Send + Sync + 'static> Default for StateTreeEventIngress<D> {
    fn default() -> Self {
        Self {
            broadcasts: Mutex::new(Vec::new()),
            dirty_entities: Mutex::new(EntityHashSet::default()),
            batch: AtomicU64::new(0),
            published_broadcasts: AtomicUsize::new(0),
            marker: PhantomData,
        }
    }
}

/// Writes one registered entity event to its target state-tree instance.
///
/// Events are queued independently for each instance. Systems that require
/// same-schedule delivery must run in [`StateTreeSet::QueueEvents`]. Concurrent
/// writers to the same entity are ordered by whichever writer acquires that
/// entity's inbox first; configure an explicit Bevy system order when that
/// ordering must be deterministic.
#[derive(SystemParam)]
pub struct EntityStateTreeEventWriter<'w, 's, D, M>
where
    D: Send + Sync + 'static,
    M: EntityEvent,
{
    binding: Res<'w, EventBinding<D, M>>,
    ingress: Res<'w, StateTreeEventIngress<D>>,
    inboxes: Query<
        'w,
        's,
        (
            &'static StateTreeInstance<D>,
            &'static StateTreeEventInbox<D>,
        ),
    >,
}

impl<D, M> EntityStateTreeEventWriter<'_, '_, D, M>
where
    D: Send + Sync + 'static,
    M: EntityEvent,
{
    /// Queues `event` and returns whether its target has a state-tree instance.
    ///
    /// The current runtime uses the value for its Rust type and entity target;
    /// event payload data is not yet exposed to state-tree transitions.
    pub fn write(&mut self, event: M) -> bool {
        let target = event.event_target();
        let Ok((instance, inbox)) = self.inboxes.get(target) else {
            return false;
        };
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
        events.push(EntityEventEntry {
            batch,
            instance,
            binding: self.binding.id,
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

/// Writes one registered event to every initialized state-tree instance.
///
/// The recipients are the instances initialized when
/// [`StateTreeSet::ProcessEvents`] runs, rather than those present when
/// [`Self::write`] is called. Systems that require same-schedule delivery must
/// run in [`StateTreeSet::QueueEvents`].
#[derive(SystemParam)]
pub struct BroadcastStateTreeEventWriter<'w, D, M>
where
    D: Send + Sync + 'static,
    M: Event,
{
    binding: Res<'w, EventBinding<D, M>>,
    ingress: Res<'w, StateTreeEventIngress<D>>,
}

impl<D, M> BroadcastStateTreeEventWriter<'_, D, M>
where
    D: Send + Sync + 'static,
    M: Event,
{
    /// Queues `event` for broadcast.
    ///
    /// The current runtime uses the value for its Rust type; event payload data
    /// is not yet exposed to state-tree transitions.
    pub fn write(&mut self, _event: M) {
        let mut broadcasts = self
            .ingress
            .broadcasts
            .lock()
            .expect("state-tree broadcast inbox poisoned");
        broadcasts.push(BroadcastEvent {
            binding: self.binding.id,
        });
        self.ingress
            .published_broadcasts
            .store(broadcasts.len(), Ordering::Release);
    }
}

fn domain_schedule<D: Send + Sync + 'static>(app: &App) -> InternedScheduleLabel {
    app.world()
        .get_resource::<DomainSchedule<D>>()
        .unwrap_or_else(|| {
            panic!(
                "StateTreePlugin<{}> must be added before registering events",
                std::any::type_name::<D>()
            )
        })
        .schedule
}

fn insert_binding<D, M>(app: &mut App, authored_name: Box<str>)
where
    D: Send + Sync + 'static,
    M: Event,
{
    assert!(
        !authored_name.is_empty(),
        "state-tree event name cannot be empty"
    );
    assert!(
        !app.world().contains_resource::<EventBinding<D, M>>(),
        "event type {} is already registered for state-tree domain {}",
        std::any::type_name::<M>(),
        std::any::type_name::<D>()
    );
    let mut bindings = app.world_mut().resource_mut::<StateTreeEventBindings<D>>();
    let id = EventBindingId(bindings.authored_names.len());
    bindings.authored_names.push(authored_name);
    app.insert_resource(EventBinding::<D, M> {
        id,
        marker: PhantomData,
    });
}

fn initialize_pending<D: Send + Sync + 'static>(
    mut commands: Commands,
    definitions: Res<Assets<StateTreeDefinition>>,
    settings: Res<StateTreeSettings<D>>,
    mut instances: Query<(Entity, &mut StateTreeInstance<D>), With<PendingStateTree<D>>>,
) {
    for (entity, mut instance) in &mut instances {
        let Some(definition) = definitions.get(instance.definition()) else {
            continue;
        };
        instance.initialize(definition.compiled.clone(), settings.max_microsteps.get());
        commands.entity(entity).remove::<PendingStateTree<D>>();
    }
}

type StateTrees<'w, 's, D> = Query<
    'w,
    's,
    (
        &'static mut StateTreeInstance<D>,
        &'static StateTreeEventInbox<D>,
    ),
>;

fn process_events<D: Send + Sync + 'static>(
    mut ingress: ResMut<StateTreeEventIngress<D>>,
    bindings: Res<StateTreeEventBindings<D>>,
    settings: Res<StateTreeSettings<D>>,
    mut instances: StateTrees<'_, '_, D>,
) {
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

    let process_instance =
        |(mut instance, inbox): (Mut<'_, StateTreeInstance<D>>, &StateTreeEventInbox<D>)| {
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
                    process_binding(
                        &mut instance,
                        broadcasts[broadcast_cursor].binding,
                        &bindings,
                        microstep_limit,
                    );
                    broadcast_cursor += 1;
                }
                process_binding(&mut instance, event.binding, &bindings, microstep_limit);
            }

            for event in &broadcasts[broadcast_cursor..] {
                process_binding(&mut instance, event.binding, &bindings, microstep_limit);
            }
        };

    let parallel = ComputeTaskPool::try_get().is_some();
    if broadcasts.is_empty() && parallel {
        instances
            .par_iter_many_unique_mut(&*dirty_entities)
            .for_each(&process_instance);
    } else if broadcasts.is_empty() {
        for instance in instances.iter_many_unique_mut(&*dirty_entities) {
            process_instance(instance);
        }
    } else if parallel {
        instances.par_iter_mut().for_each(&process_instance);
    } else {
        for instance in &mut instances {
            process_instance(instance);
        }
    }

    broadcasts.clear();
    dirty_entities.clear();
    *ingress.batch.get_mut() = batch.wrapping_add(1);
    *ingress.published_broadcasts.get_mut() = 0;
}

fn process_binding<D: Send + Sync + 'static>(
    instance: &mut Mut<'_, StateTreeInstance<D>>,
    binding: EventBindingId,
    bindings: &StateTreeEventBindings<D>,
    microstep_limit: usize,
) {
    let changed = instance.bypass_change_detection().process_event(
        &bindings.authored_names[binding.0],
        &(),
        microstep_limit,
    );
    if changed {
        instance.set_changed();
    }
}

#[cfg(test)]
mod tests {
    use bevy_app::{App, TaskPoolPlugin, Update};
    use bevy_asset::{AssetPlugin, Assets};
    use bevy_ecs::{
        event::{EntityEvent, Event},
        prelude::{Changed, Entity},
        schedule::ScheduleBuildSettings,
        system::{In, RunSystemOnce},
    };

    use super::*;
    use crate::{InstanceStatus, StateTreeBuilder};

    struct Simulation;

    #[derive(EntityEvent)]
    struct Advance {
        entity: Entity,
    }

    #[derive(EntityEvent)]
    struct Reset {
        entity: Entity,
    }

    #[derive(Event)]
    struct ResetAll;

    #[derive(Event)]
    struct AdvanceAll;

    #[derive(Resource, Default)]
    struct ChangedInstances(usize);

    fn definition() -> StateTreeDefinition {
        let mut builder = StateTreeBuilder::new("messages");
        let root = builder.compound_root("root").unwrap();
        let idle = builder.atomic("idle", root).unwrap();
        let active = builder.atomic("active", root).unwrap();
        builder.initial(root, idle).unwrap();
        builder.transition(idle, "advance", active).unwrap();
        builder.transition(active, "reset", idle).unwrap();
        builder.build().unwrap()
    }

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins((
            TaskPoolPlugin::default(),
            AssetPlugin::default(),
            StateTreePlugin::<Simulation>::new(Update),
        ))
        .register_entity_state_tree_event::<Simulation, Advance>("advance")
        .register_entity_state_tree_event::<Simulation, Reset>("reset")
        .register_broadcast_state_tree_event::<Simulation, ResetAll>("reset")
        .register_broadcast_state_tree_event::<Simulation, AdvanceAll>("advance");
        app
    }

    #[test]
    fn plugin_preserves_an_existing_definition_asset_store() {
        let mut app = App::new();
        app.add_plugins(AssetPlugin::default())
            .init_asset::<StateTreeDefinition>();
        let handle = app
            .world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .add(definition());

        app.add_plugins(StateTreePlugin::<Simulation>::new(Update));

        assert!(
            app.world()
                .resource::<Assets<StateTreeDefinition>>()
                .contains(handle.id())
        );
    }

    fn write_advance(
        In(event): In<Advance>,
        mut writer: EntityStateTreeEventWriter<Simulation, Advance>,
    ) -> bool {
        writer.write(event)
    }

    fn write_reset(
        In(event): In<Reset>,
        mut writer: EntityStateTreeEventWriter<Simulation, Reset>,
    ) -> bool {
        writer.write(event)
    }

    fn write_reset_all(mut writer: BroadcastStateTreeEventWriter<Simulation, ResetAll>) {
        writer.write(ResetAll);
    }

    fn write_advance_all(mut writer: BroadcastStateTreeEventWriter<Simulation, AdvanceAll>) {
        writer.write(AdvanceAll);
    }

    fn record_changed_instances(
        instances: Query<(), Changed<StateTreeInstance<Simulation>>>,
        mut changed: ResMut<ChangedInstances>,
    ) {
        changed.0 = instances.iter().count();
    }

    fn queue_advance(app: &mut App, entity: Entity) {
        assert!(
            app.world_mut()
                .run_system_once_with(write_advance, Advance { entity })
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
            .get::<StateTreeInstance<Simulation>>(entity)
            .unwrap()
            .active_state_names()
            .last()
            .unwrap()
            .to_owned()
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
            .spawn(StateTreeInstance::<Simulation>::new(handle.clone()))
            .id();
        let second = app
            .world_mut()
            .spawn(StateTreeInstance::<Simulation>::new(handle))
            .id();
        app.update();

        queue_advance(&mut app, first);
        app.update();

        assert_eq!(active_leaf(&app, first), "active");
        assert_eq!(active_leaf(&app, second), "idle");
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
            .spawn(StateTreeInstance::<Simulation>::new(handle.clone()))
            .id();
        let second = app
            .world_mut()
            .spawn(StateTreeInstance::<Simulation>::new(handle))
            .id();
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
    fn entity_events_preserve_order_across_rust_types() {
        let mut app = app();
        let handle = app
            .world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .add(definition());
        let entity = app
            .world_mut()
            .spawn(StateTreeInstance::<Simulation>::new(handle))
            .id();
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
        let entity = app
            .world_mut()
            .spawn(StateTreeInstance::<Simulation>::new(handle))
            .id();
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
            .spawn(StateTreeInstance::<Simulation>::new(handle.clone()))
            .id();
        let second = app
            .world_mut()
            .spawn(StateTreeInstance::<Simulation>::new(handle))
            .id();
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
            .spawn(StateTreeInstance::<Simulation>::new(handle.clone()))
            .id();
        let second = app
            .world_mut()
            .spawn(StateTreeInstance::<Simulation>::new(handle))
            .id();

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
            .spawn(StateTreeInstance::<Simulation>::new(handle.clone()))
            .id();

        queue_advance(&mut app, entity);
        queue_advance_all(&mut app);
        app.update();
        assert_eq!(
            app.world()
                .get::<StateTreeInstance<Simulation>>(entity)
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
            .spawn(StateTreeInstance::<Simulation>::new(handle.clone()))
            .id();
        app.update();

        queue_advance(&mut app, entity);
        assert!(
            app.world()
                .get::<StateTreeEventInbox<Simulation>>(entity)
                .unwrap()
                .events
                .lock()
                .unwrap()
                .capacity()
                > 0
        );
        app.world_mut()
            .entity_mut(entity)
            .remove::<StateTreeInstance<Simulation>>();
        assert_eq!(
            app.world()
                .get::<StateTreeEventInbox<Simulation>>(entity)
                .unwrap()
                .events
                .lock()
                .unwrap()
                .capacity(),
            0
        );
        app.world_mut()
            .entity_mut(entity)
            .insert(StateTreeInstance::<Simulation>::new(handle));
        app.update();

        assert_eq!(active_leaf(&app, entity), "idle");
    }

    #[test]
    fn an_inbox_detached_for_a_dispatch_does_not_replay_its_old_batch() {
        let mut app = app();
        let handle = app
            .world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .add(definition());
        let entity = app
            .world_mut()
            .spawn(StateTreeInstance::<Simulation>::new(handle))
            .id();
        app.update();

        queue_reset_all(&mut app);
        queue_advance(&mut app, entity);
        let instance = app
            .world_mut()
            .entity_mut(entity)
            .take::<StateTreeInstance<Simulation>>()
            .unwrap();
        app.update();

        app.world_mut().entity_mut(entity).insert(instance);
        queue_reset_all(&mut app);
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
            .spawn(StateTreeInstance::<Simulation>::new(ready_handle))
            .id();
        let pending_handle = app
            .world()
            .resource::<Assets<StateTreeDefinition>>()
            .reserve_handle();
        let pending = app
            .world_mut()
            .spawn(StateTreeInstance::<Simulation>::new(pending_handle))
            .id();

        app.update();

        assert_eq!(
            app.world()
                .get::<StateTreeInstance<Simulation>>(ready)
                .unwrap()
                .status(),
            &InstanceStatus::Running
        );
        assert_eq!(
            app.world()
                .get::<StateTreeInstance<Simulation>>(pending)
                .unwrap()
                .status(),
            &InstanceStatus::Pending
        );
    }
}
