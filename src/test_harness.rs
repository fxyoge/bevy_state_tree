use std::{num::NonZeroUsize, time::Duration};

use bevy_app::{App, Update};
use bevy_asset::{AssetPlugin, Assets};
use bevy_ecs::{
    event::Event,
    system::{In, RunSystemOnce},
};
use bevy_time::{Time, TimePlugin};

use crate::{
    BroadcastStateTreeEvent, InstanceStatus, LuauValue, StateTreeDataOverrides,
    StateTreeDefinition, StateTreeInstance, StateTreePlugin, StateTreeSettings, StateTreeTraceLog,
    StateTreeTracePlugin,
};

pub(crate) struct TestApp {
    app: App,
    entity: bevy_ecs::entity::Entity,
}

impl TestApp {
    pub(crate) fn new(
        configure: fn(&mut crate::StateTreeConfig<'_>),
        definition: StateTreeDefinition,
        limit: NonZeroUsize,
    ) -> Self {
        Self::with_initial_data(
            configure,
            definition,
            limit,
            StateTreeDataOverrides::default(),
        )
    }

    pub(crate) fn with_initial_data(
        configure: fn(&mut crate::StateTreeConfig<'_>),
        definition: StateTreeDefinition,
        limit: NonZeroUsize,
        initial_data: StateTreeDataOverrides,
    ) -> Self {
        let mut app = App::new();
        app.add_plugins((
            TimePlugin,
            AssetPlugin::default(),
            StateTreeTracePlugin,
            StateTreePlugin::new(Update).configure(configure),
        ));
        app.insert_resource(StateTreeSettings::new(limit));
        let handle = app
            .world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .add(definition);
        let entity = app
            .world_mut()
            .spawn(StateTreeInstance::new_with_initial_data(
                handle,
                initial_data,
            ))
            .id();
        app.update();
        Self { app, entity }
    }

    pub(crate) fn instance(&self) -> &StateTreeInstance {
        self.app
            .world()
            .get(self.entity)
            .expect("test state-tree instance exists")
    }

    pub(crate) fn status(&self) -> &InstanceStatus {
        self.instance().status()
    }

    pub(crate) fn active_state_names(&self) -> impl Iterator<Item = &str> {
        self.instance().active_state_names()
    }

    pub(crate) fn data(&self, name: &str) -> Option<&LuauValue> {
        self.instance().data(name)
    }

    pub(crate) fn process_event<E>(&mut self, event: E) -> bool
    where
        E: BroadcastStateTreeEvent,
    {
        let before = self.trace().as_str().matches("transition ").count();
        self.app
            .world_mut()
            .run_system_once_with(queue_broadcast::<E>, event)
            .expect("test event writer system runs");
        self.app.world_mut().run_schedule(Update);
        before != self.trace().as_str().matches("transition ").count()
    }

    pub(crate) fn advance_time(&mut self, duration: Duration) -> bool {
        let before = self.trace().as_str().matches("macrostep timer\n").count();
        self.app
            .world_mut()
            .resource_mut::<Time>()
            .advance_by(duration);
        self.app.world_mut().run_schedule(Update);
        before != self.trace().as_str().matches("macrostep timer\n").count()
    }

    pub(crate) fn trace(&self) -> crate::StateTreeEventLog {
        self.app
            .world()
            .resource::<StateTreeTraceLog>()
            .get(self.entity)
            .expect("test instance has a trace")
    }

    pub(crate) fn app(&self) -> &App {
        &self.app
    }

    pub(crate) fn app_mut(&mut self) -> &mut App {
        &mut self.app
    }

    pub(crate) fn entity(&self) -> bevy_ecs::entity::Entity {
        self.entity
    }
}

fn queue_broadcast<E>(In(event): In<E>, mut writer: crate::BroadcastStateTreeEventWriter)
where
    E: BroadcastStateTreeEvent + Event,
{
    writer.write(event);
}
