#![cfg(feature = "persistence")]
use bevy_app::App;
use bevy_ecs::prelude::*;
use bevy_gigasaver::{LoadOutcome, SaveOpened, SavePlugin, SaveRequest, SaveSettings, SaveStatus};
use bevy_state_tree::persistence::AppPersistenceExt;
use bevy_time::{Real, Time, TimePlugin};
use serde::{Deserialize, Serialize};
use std::{path::Path, time::Duration};

#[derive(Resource, Default, Serialize, Deserialize)]
struct Score(u64);
#[derive(Resource)]
struct Ready(bool);

fn app() -> App {
    let mut app = App::new();
    app.add_plugins(TimePlugin)
        .insert_resource(Ready(true))
        .init_resource::<Score>()
        .persist_resource::<Score>("score")
        .add_plugins(SavePlugin::default().enabled_when(|ready: Res<Ready>| ready.0))
        .add_systems(SaveOpened, |world: &mut World| {
            if world.resource::<SaveStatus>().load() == Some(&Ok(LoadOutcome::Missing)) {
                world.insert_resource(Score(10));
            }
        });
    app
}

fn request(app: &mut App, request: SaveRequest) {
    app.world_mut().write_message(request);
    app.update();
    app.update();
}

fn open(app: &mut App, path: &Path) {
    request(app, SaveRequest::Open(path.into()));
}

#[test]
fn never_suppresses_automatic_saves_but_manual_saves_restore_in_a_fresh_app() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("slot.sqlite");
    let mut first = app();
    open(&mut first, &path);
    first.world_mut().resource_mut::<Score>().0 = 93;
    first
        .world_mut()
        .resource_mut::<Time<Real>>()
        .advance_by(Duration::from_secs(3600));
    first.update();
    assert!(!path.exists());
    request(&mut first, SaveRequest::Save);
    assert!(
        first
            .world()
            .resource::<SaveStatus>()
            .last_save()
            .unwrap()
            .is_ok()
    );
    let mut second = app();
    open(&mut second, &path);
    assert_eq!(
        second.world().resource::<SaveStatus>().load(),
        Some(&Ok(LoadOutcome::Loaded))
    );
    assert_eq!(second.world().resource::<Score>().0, 93);
}

#[test]
fn settings_changes_reset_deadlines_and_never_cancels_an_existing_deadline() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("slot.sqlite");
    let mut app = app();
    open(&mut app, &path);
    app.world_mut().resource_mut::<SaveSettings>().interval = Some(Duration::from_secs(10));
    app.update();
    app.world_mut()
        .resource_mut::<Time<Real>>()
        .advance_by(Duration::from_secs(11));
    app.update();
    assert!(path.exists());
    let generation = app
        .world()
        .resource::<SaveStatus>()
        .last_save()
        .unwrap()
        .as_ref()
        .unwrap()
        .generation;
    app.world_mut().resource_mut::<SaveSettings>().interval = None;
    app.update();
    app.world_mut()
        .resource_mut::<Time<Real>>()
        .advance_by(Duration::from_secs(3600));
    app.update();
    assert_eq!(
        app.world()
            .resource::<SaveStatus>()
            .last_save()
            .unwrap()
            .as_ref()
            .unwrap()
            .generation,
        generation
    );
}

#[test]
fn failed_outgoing_save_preserves_session_and_blocks_close_and_slot_switch() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("first.sqlite");
    let other = directory.path().join("second.sqlite");
    let mut app = app();
    open(&mut app, &path);
    app.world_mut().resource_mut::<Score>().0 = 42;
    app.world_mut().resource_mut::<Ready>().0 = false;
    request(&mut app, SaveRequest::Close);
    assert_eq!(app.world().resource::<Score>().0, 42);
    request(&mut app, SaveRequest::Open(other));
    assert_eq!(
        app.world().resource::<SaveStatus>().path(),
        Some(path.as_path())
    );
    assert_eq!(app.world().resource::<Score>().0, 42);
    app.world_mut().resource_mut::<Ready>().0 = true;
    request(&mut app, SaveRequest::Close);
    assert_eq!(app.world().resource::<SaveStatus>().path(), None);
    let mut restored = self::app();
    open(&mut restored, &path);
    assert_eq!(restored.world().resource::<Score>().0, 42);
}

#[test]
fn broken_destination_retains_outgoing_world_and_original_save_path() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("first.sqlite");
    let broken = directory.path().join("broken.sqlite");
    std::fs::write(&broken, b"not a database").unwrap();
    let mut app = app();
    open(&mut app, &path);
    app.world_mut().resource_mut::<Score>().0 = 21;
    request(&mut app, SaveRequest::Open(broken.clone()));
    assert!(
        app.world()
            .resource::<SaveStatus>()
            .load()
            .unwrap()
            .is_err()
    );
    assert_eq!(
        app.world().resource::<SaveStatus>().path(),
        Some(path.as_path())
    );
    assert_eq!(app.world().resource::<Score>().0, 21);
    assert_eq!(std::fs::read(broken).unwrap(), b"not a database");
}

#[test]
fn external_exit_commits_before_exiting_even_when_autosave_is_never() {
    use bevy_app::AppExit;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("exit.sqlite");
    let mut app = app();
    open(&mut app, &path);
    app.world_mut().resource_mut::<Score>().0 = 83;
    app.world_mut().write_message(AppExit::Success);
    app.update();
    assert!(app.should_exit().is_some());
    assert!(path.exists());
    let mut restored = self::app();
    open(&mut restored, &path);
    assert_eq!(restored.world().resource::<Score>().0, 83);
}

#[test]
fn preparation_commands_get_a_normal_cleanup_frame_before_capture() {
    use bevy_app::Update;
    use bevy_gigasaver::PrepareSave;
    #[derive(Component, Serialize, Deserialize)]
    struct Owner;
    #[derive(Resource, Default, Serialize, Deserialize)]
    struct Owners(
        #[serde(with = "bevy_state_tree::persistence::entity_map")]
        bevy_ecs::entity::EntityHashMap<()>,
    );
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("prepared.sqlite");
    let mut app = app();
    app.persist_component::<Owner>("owner")
        .persist_resource::<Owners>("owners")
        .init_resource::<Owners>()
        .add_systems(
            PrepareSave,
            |owners: Query<Entity, With<Owner>>, mut commands: Commands| {
                for entity in &owners {
                    commands.entity(entity).despawn();
                }
            },
        )
        .add_systems(
            Update,
            |mut removed: RemovedComponents<Owner>, mut owners: ResMut<Owners>| {
                for entity in removed.read() {
                    owners.0.remove(&entity);
                }
            },
        );
    open(&mut app, &path);
    let owner = app.world_mut().spawn(Owner).id();
    app.world_mut()
        .insert_resource(Owners([(owner, ())].into()));
    app.world_mut().write_message(SaveRequest::Save);
    app.update();
    assert!(!path.exists());
    app.update();
    assert!(
        app.world()
            .resource::<SaveStatus>()
            .last_save()
            .unwrap()
            .is_ok()
    );
    assert!(app.world().resource::<Owners>().0.is_empty());
}
