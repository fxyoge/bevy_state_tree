use bevy_app::{App, Update};
use bevy_asset::{AssetPlugin, Assets};
use bevy_ecs::prelude::*;
use bevy_state_tree::{persistence::AppPersistenceExt, prelude::*};
use bevy_state_tree_gigasaver::{Error, GigasaverStore, LoadOutcome};
use bevy_time::TimePlugin;
use serde::{Deserialize, Serialize};

struct Domain;
impl StateTreeDomain for Domain {
    const AUTHORED_NAME: &'static str = "storage-test";
    fn configure(_: &mut StateTreeConfig<'_, Self>) {}
}

#[derive(Resource, Default, Serialize, Deserialize)]
struct Score(u64);

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((
        TimePlugin,
        AssetPlugin::default(),
        StateTreePlugin::<Domain>::new(Update),
    ))
    .init_resource::<Score>()
    .persist_resource::<Score>("score");
    let mut definition = StateTreeBuilder::new("storage-tree");
    let root = definition.compound_root("root").unwrap();
    let idle = definition.atomic("idle", root).unwrap();
    definition.initial(root, idle).unwrap();
    let handle = app
        .world_mut()
        .resource_mut::<Assets<StateTreeDefinition>>()
        .add(definition.build().unwrap());
    app.world_mut()
        .spawn(StateTreeInstance::<Domain>::new(handle));
    app.world_mut().run_schedule(Update);
    app
}

#[test]
fn sqlite_round_trip_uses_an_independent_backend_without_game_gigasaver_registration() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("save.sqlite");
    let mut store = GigasaverStore::open(&path).unwrap();
    let mut source = app();
    assert_eq!(
        store.load(source.world_mut()).unwrap(),
        LoadOutcome::Missing
    );
    source.world_mut().resource_mut::<Score>().0 = 901;
    store.save(source.world_mut()).unwrap();
    drop(store);
    let mut target = app();
    let mut store = GigasaverStore::open(&path).unwrap();
    assert!(matches!(
        store.save(target.world_mut()),
        Err(Error::LoadRequired)
    ));
    assert_eq!(store.load(target.world_mut()).unwrap(), LoadOutcome::Loaded);
    assert_eq!(target.world().resource::<Score>().0, 901);
    assert_eq!(
        target
            .world_mut()
            .query::<&StateTreeInstance<Domain>>()
            .iter(target.world())
            .count(),
        1
    );
    target.world_mut().resource_mut::<Score>().0 = 902;
    store.save(target.world_mut()).unwrap();
    store.load(source.world_mut()).unwrap();
    assert_eq!(source.world().resource::<Score>().0, 902);
}

#[test]
fn incompatible_checkpoint_does_not_replace_game_state_or_authorize_overwrite() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("save.sqlite");
    let mut source = app();
    let mut store = GigasaverStore::open(&path).unwrap();
    store.save(source.world_mut()).unwrap();
    let mut target = app();
    target.persist_resource::<Extra>("extra");
    target.world_mut().resource_mut::<Score>().0 = 77;
    assert!(store.load(target.world_mut()).is_err());
    assert_eq!(target.world().resource::<Score>().0, 77);
    assert!(matches!(
        store.save(target.world_mut()),
        Err(Error::LoadRequired)
    ));
}

#[derive(Resource, Serialize, Deserialize)]
struct Extra;
