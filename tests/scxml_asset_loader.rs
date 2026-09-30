use bevy_app::{App, TaskPoolPlugin};
use bevy_asset::{AssetPlugin, AssetServer, Assets, LoadState};
use bevy_ecs::schedule::ScheduleLabel;
use bevy_state_tree::prelude::*;
use bevy_time::TimePlugin;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, ScheduleLabel)]
struct StateTreeUpdate;

struct TestDomain;

impl StateTreeDomain for TestDomain {
    fn configure(_config: &mut StateTreeConfig<'_, Self>) {}
}

fn test_app() -> App {
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
    ))
    .init_schedule(StateTreeUpdate)
    .add_plugins(StateTreePlugin::<TestDomain>::new(StateTreeUpdate));
    app
}

fn update(app: &mut App) {
    app.update();
    app.world_mut().run_schedule(StateTreeUpdate);
    std::thread::yield_now();
}

#[test]
fn plugin_loads_scxml_assets_and_initializes_instances() {
    let mut app = test_app();
    let handle = app
        .world()
        .resource::<AssetServer>()
        .load::<StateTreeDefinition>("loaded.scxml");
    let entity = app
        .world_mut()
        .spawn(StateTreeInstance::<TestDomain>::new(handle.clone()))
        .id();

    for _ in 0..1_000 {
        update(&mut app);
        if app
            .world()
            .get::<StateTreeInstance<TestDomain>>(entity)
            .is_some_and(|instance| instance.status() == &InstanceStatus::Running)
        {
            break;
        }
    }

    let definition = app
        .world()
        .resource::<Assets<StateTreeDefinition>>()
        .get(&handle)
        .expect("SCXML definition should load");
    assert_eq!(definition.name(), "loaded");
    assert_eq!(definition.state_count(), 2);

    let instance = app
        .world()
        .get::<StateTreeInstance<TestDomain>>(entity)
        .expect("state-tree instance should remain present");
    assert_eq!(instance.status(), &InstanceStatus::Running);
    assert_eq!(instance.active_state_names().last(), Some("idle"));
}

#[test]
fn parser_and_compiler_errors_fail_asset_loading() {
    let mut app = test_app();
    let invalid = app
        .world()
        .resource::<AssetServer>()
        .load::<StateTreeDefinition>("invalid.scxml");
    let unsupported = app
        .world()
        .resource::<AssetServer>()
        .load::<StateTreeDefinition>("unsupported.scxml");

    let mut invalid_error = None;
    let mut unsupported_error = None;
    for _ in 0..1_000 {
        update(&mut app);
        let server = app.world().resource::<AssetServer>();
        if let LoadState::Failed(error) = server.load_state(invalid.id()) {
            invalid_error = Some(error);
        }
        if let LoadState::Failed(error) = server.load_state(unsupported.id()) {
            unsupported_error = Some(error);
        }
        if invalid_error.is_some() && unsupported_error.is_some() {
            break;
        }
    }

    let invalid_error = invalid_error.expect("invalid XML should fail to load");
    assert!(invalid_error.to_string().contains("invalid.scxml"));
    let unsupported_error = unsupported_error.expect("unsupported SCXML should fail to load");
    assert!(
        unsupported_error
            .to_string()
            .contains("unsupported SCXML feature: deep history states")
    );
}

#[test]
fn event_protocol_linking_remains_deferred_until_initialization() {
    let mut app = test_app();
    let handle = app
        .world()
        .resource::<AssetServer>()
        .load::<StateTreeDefinition>("unlinked.scxml");
    let entity = app
        .world_mut()
        .spawn(StateTreeInstance::<TestDomain>::new(handle.clone()))
        .id();

    for _ in 0..1_000 {
        update(&mut app);
        if app
            .world()
            .get::<StateTreeLinkFailure<TestDomain>>(entity)
            .is_some()
        {
            break;
        }
    }

    assert!(
        app.world()
            .resource::<Assets<StateTreeDefinition>>()
            .contains(&handle),
        "domain-independent definition should load successfully"
    );
    let instance = app
        .world()
        .get::<StateTreeInstance<TestDomain>>(entity)
        .expect("state-tree instance should remain present");
    assert_eq!(instance.status(), &InstanceStatus::Pending);

    let failure = app
        .world()
        .get::<StateTreeLinkFailure<TestDomain>>(entity)
        .expect("domain linking should reject the unknown event");
    assert!(matches!(
        failure.error().issues(),
        [StateTreeLinkIssue::UnmatchedEventDescriptor { descriptor, .. }]
            if descriptor.as_ref() == "missing"
    ));
}
