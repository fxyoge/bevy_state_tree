use std::{marker::PhantomData, num::NonZeroUsize};

use bevy_app::{App, Plugin};
use bevy_asset::{AssetApp, AssetServer, Assets};
use bevy_ecs::{
    event::EntityEvent,
    message::{Message, MessageReader},
    prelude::{Commands, Entity, Query, Res, Resource, With, Without},
    schedule::{InternedScheduleLabel, IntoScheduleConfigs, ScheduleLabel, SystemSet},
};

use crate::{StateTreeDefinition, StateTreeInstance, instance::PendingStateTree};

/// Ordered phases installed into each state-tree domain schedule.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, SystemSet)]
pub enum StateTreeSet {
    /// Initializes instances whose definition assets are available.
    Initialize,
    /// Delivers registered Bevy messages and runs macrosteps to stability.
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
            app.init_asset::<StateTreeDefinition>();
            app.insert_resource(StateTreeAssetsInstalled);
        }

        app.insert_resource(DomainSchedule::<D> {
            schedule: self.schedule,
            marker: PhantomData,
        })
        .init_resource::<StateTreeSettings<D>>()
        .configure_sets(
            self.schedule,
            (StateTreeSet::Initialize, StateTreeSet::ProcessEvents).chain(),
        )
        .add_systems(
            self.schedule,
            initialize_pending::<D>.in_set(StateTreeSet::Initialize),
        );
    }
}

/// Registers ordinary Bevy messages as statechart events.
pub trait StateTreeAppExt {
    /// Delivers each message only to the instance on its entity-event target.
    fn register_targeted_state_tree_event<D, M>(
        &mut self,
        authored_name: impl Into<Box<str>>,
    ) -> &mut Self
    where
        D: Send + Sync + 'static,
        M: Message + EntityEvent;

    /// Delivers each message to every initialized instance in the domain.
    fn register_broadcast_state_tree_event<D, M>(
        &mut self,
        authored_name: impl Into<Box<str>>,
    ) -> &mut Self
    where
        D: Send + Sync + 'static,
        M: Message;
}

impl StateTreeAppExt for App {
    fn register_targeted_state_tree_event<D, M>(
        &mut self,
        authored_name: impl Into<Box<str>>,
    ) -> &mut Self
    where
        D: Send + Sync + 'static,
        M: Message + EntityEvent,
    {
        let schedule = domain_schedule::<D>(self);
        insert_binding::<D, M>(self, authored_name.into());
        self.add_message::<M>().add_systems(
            schedule,
            process_targeted::<D, M>.in_set(StateTreeSet::ProcessEvents),
        )
    }

    fn register_broadcast_state_tree_event<D, M>(
        &mut self,
        authored_name: impl Into<Box<str>>,
    ) -> &mut Self
    where
        D: Send + Sync + 'static,
        M: Message,
    {
        let schedule = domain_schedule::<D>(self);
        insert_binding::<D, M>(self, authored_name.into());
        self.add_message::<M>().add_systems(
            schedule,
            process_broadcast::<D, M>.in_set(StateTreeSet::ProcessEvents),
        )
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
struct EventBinding<D: Send + Sync + 'static, M: Message> {
    authored_name: Box<str>,
    marker: PhantomData<fn() -> (D, M)>,
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
    M: Message,
{
    assert!(
        !authored_name.is_empty(),
        "state-tree event name cannot be empty"
    );
    assert!(
        !app.world().contains_resource::<EventBinding<D, M>>(),
        "message type {} is already registered for state-tree domain {}",
        std::any::type_name::<M>(),
        std::any::type_name::<D>()
    );
    app.insert_resource(EventBinding::<D, M> {
        authored_name,
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

fn process_targeted<D, M>(
    mut messages: MessageReader<M>,
    binding: Res<EventBinding<D, M>>,
    settings: Res<StateTreeSettings<D>>,
    mut instances: Query<&mut StateTreeInstance<D>, Without<PendingStateTree<D>>>,
) where
    D: Send + Sync + 'static,
    M: Message + EntityEvent,
{
    for message in messages.read() {
        let Ok(mut instance) = instances.get_mut(message.event_target()) else {
            continue;
        };
        instance.process_event(
            &binding.authored_name,
            message,
            settings.max_microsteps.get(),
        );
    }
}

fn process_broadcast<D, M>(
    mut messages: MessageReader<M>,
    binding: Res<EventBinding<D, M>>,
    settings: Res<StateTreeSettings<D>>,
    mut instances: Query<&mut StateTreeInstance<D>, Without<PendingStateTree<D>>>,
) where
    D: Send + Sync + 'static,
    M: Message,
{
    let messages = messages.read().collect::<Vec<_>>();
    if messages.is_empty() {
        return;
    }

    instances.par_iter_mut().for_each(|mut instance| {
        for message in &messages {
            instance.process_event(
                &binding.authored_name,
                *message,
                settings.max_microsteps.get(),
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use bevy_app::{App, TaskPoolPlugin, Update};
    use bevy_asset::{AssetPlugin, Assets, Handle};
    use bevy_ecs::{event::EntityEvent, message::Message, prelude::Entity};

    use super::*;
    use crate::{InstanceStatus, StateTreeBuilder};

    struct Simulation;

    #[derive(Clone, Copy, EntityEvent, Message)]
    struct Advance {
        entity: Entity,
    }

    #[derive(Clone, Copy, Message)]
    struct ResetAll;

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
        .register_targeted_state_tree_event::<Simulation, Advance>("advance")
        .register_broadcast_state_tree_event::<Simulation, ResetAll>("reset");
        app
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
    fn targeted_messages_only_advance_the_target_instance() {
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

        app.world_mut().write_message(Advance { entity: first });
        app.update();

        assert_eq!(active_leaf(&app, first), "active");
        assert_eq!(active_leaf(&app, second), "idle");
    }

    #[test]
    fn broadcast_messages_advance_every_initialized_instance() {
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
        app.world_mut().write_message(Advance { entity: first });
        app.world_mut().write_message(Advance { entity: second });
        app.update();
        assert_eq!(active_leaf(&app, first), "active");
        assert_eq!(active_leaf(&app, second), "active");

        app.world_mut().write_message(ResetAll);
        app.update();

        assert_eq!(active_leaf(&app, first), "idle");
        assert_eq!(active_leaf(&app, second), "idle");
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
        let pending = app
            .world_mut()
            .spawn(StateTreeInstance::<Simulation>::new(Handle::default()))
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
