//! Annotated data and game-specific fresh-world population.
//! `cargo run --features persistence --example save_slots -- /tmp/slot.sqlite`
#![cfg(feature = "persistence")]
use bevy_app::{App, TaskPoolPlugin, Update};
use bevy_asset::{AssetPlugin, Assets, Handle};
use bevy_ecs::prelude::*;
use bevy_gigasaver::{LoadOutcome, SaveOpened, SavePlugin, SaveRequest, SaveSettings, SaveStatus};
use bevy_state_tree::prelude::*;
use bevy_time::TimePlugin;
use serde::{Deserialize, Serialize};

struct Simulation;
impl StateTreeDomain for Simulation {
    const AUTHORED_NAME: &'static str = "example";
    fn configure(_: &mut StateTreeConfig<'_, Self>) {}
}

#[bevy_gigasaver::save(resource, key = "example.score")]
#[derive(Resource, Default, Serialize, Deserialize)]
struct Score(u64);
#[derive(Resource)]
struct Definition(Handle<StateTreeDefinition>);

fn fresh(status: Res<SaveStatus>, definition: Res<Definition>, mut commands: Commands) {
    if status.load() == Some(&Ok(LoadOutcome::Missing)) {
        commands.insert_resource(Score(10));
        commands.spawn(StateTreeInstance::<Simulation>::new(definition.0.clone()));
    }
}

fn main() {
    let path = std::env::args().nth(1).expect("pass a SQLite slot path");
    let mut app = App::new();
    app.add_plugins((
        TaskPoolPlugin::default(),
        TimePlugin,
        AssetPlugin::default(),
        StateTreePlugin::<Simulation>::new(Update),
    ))
    .insert_resource(SaveSettings { interval: None }) // Never; manual requests still commit.
    .add_plugins(SavePlugin::default())
    .add_systems(SaveOpened, fresh);
    let mut builder = StateTreeBuilder::new("example");
    let root = builder.compound_root("root").unwrap();
    let idle = builder.atomic("idle", root).unwrap();
    builder.initial(root, idle).unwrap();
    let definition = app
        .world_mut()
        .resource_mut::<Assets<StateTreeDefinition>>()
        .add(builder.build().unwrap());
    app.insert_resource(Definition(definition));
    app.world_mut()
        .write_message(SaveRequest::Open(path.into()));
    app.update();
    assert!(app.world().resource::<SaveStatus>().can_save());
    app.world_mut().resource_mut::<Score>().0 += 1;
    app.world_mut().write_message(SaveRequest::Save);
    app.update();
    app.update();
    assert!(
        app.world()
            .resource::<SaveStatus>()
            .last_save()
            .unwrap()
            .is_ok()
    );
    println!("saved score {}", app.world().resource::<Score>().0);
}
