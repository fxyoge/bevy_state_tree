use bevy_app::App;
use bevy_asset::{AssetPlugin, Assets};
use bevy_ecs::{
    prelude::*,
    schedule::{ScheduleLabel, SingleThreadedExecutor},
};
use bevy_state_tree::prelude::*;
use bevy_time::TimePlugin;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, ScheduleLabel)]
struct StateTreeUpdate;

struct Simulation;

impl StateTreeDomain for Simulation {
    fn configure(config: &mut StateTreeConfig<'_, Self>) {
        config.event::<Advance>();
    }
}

#[derive(EntityEvent, StateTreeEvent)]
#[state_tree(domain = Simulation, name = "advance")]
struct Advance {
    entity: Entity,
}

#[derive(Resource)]
struct Target(Entity);

fn queue_advance(target: Res<Target>, mut events: EntityStateTreeEventWriter<Simulation>) {
    assert!(events.write(Advance { entity: target.0 }));
}

#[test]
fn queue_set_delivers_on_a_single_threaded_schedule_without_task_pool_plugin() {
    let mut app = App::new();
    app.add_plugins((TimePlugin, AssetPlugin::default()))
        .init_schedule(StateTreeUpdate)
        .add_plugins(StateTreePlugin::<Simulation>::new(StateTreeUpdate))
        .add_systems(
            StateTreeUpdate,
            queue_advance.in_set(StateTreeSet::QueueEvents),
        );
    app.edit_schedule(StateTreeUpdate, |schedule| {
        schedule.set_executor(SingleThreadedExecutor::new());
    });

    let mut builder = StateTreeBuilder::new("single-threaded");
    let root = builder.compound_root("root").unwrap();
    let idle = builder.atomic("idle", root).unwrap();
    let active = builder.atomic("active", root).unwrap();
    builder.initial(root, idle).unwrap();
    builder
        .add_transition(idle, StateTreeTransition::on("advance").target(active))
        .unwrap();
    let handle = app
        .world_mut()
        .resource_mut::<Assets<StateTreeDefinition>>()
        .add(builder.build().unwrap());
    let entity = app
        .world_mut()
        .spawn(StateTreeInstance::<Simulation>::new(handle))
        .id();
    app.insert_resource(Target(entity));

    app.world_mut().run_schedule(StateTreeUpdate);

    assert_eq!(
        app.world()
            .get::<StateTreeInstance<Simulation>>(entity)
            .unwrap()
            .active_state_names()
            .last(),
        Some("active")
    );
}
