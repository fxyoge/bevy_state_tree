use std::{collections::HashMap, marker::PhantomData};

use bevy_app::App;
use bevy_asset::{AssetLoadFailedEvent, AssetPath, AssetServer, Assets, Handle};
use bevy_ecs::{
    entity::Entity,
    message::MessageReader,
    prelude::{Commands, Query, Res, ResMut, Resource},
    schedule::{InternedScheduleLabel, IntoScheduleConfigs},
};

use crate::{
    InstanceStatus, LuauValue, StateTreeActivity, StateTreeActivityLifecycle,
    StateTreeActivityOutcomeWriter, StateTreeActivityToken, StateTreeDefinition, StateTreeDomain,
    StateTreeInstance, StateTreeLinkFailure, StateTreeSet,
};

const NAME: &str = "bst.spawn";

struct SpawnActivity {
    definition: String,
}

impl StateTreeActivity for SpawnActivity {
    type Output = ();
    type Error = String;

    fn error_data(
        error: &Self::Error,
    ) -> Result<Option<LuauValue>, crate::StateTreeEventDataError> {
        Ok(Some(LuauValue::String(error.clone().into())))
    }
}

enum SpawnRunState {
    Loading(Handle<StateTreeDefinition>),
    Running(Entity),
}

struct SpawnRun<D: StateTreeDomain> {
    token: StateTreeActivityToken<D, SpawnActivity>,
    definition: String,
    state: SpawnRunState,
}

#[derive(Resource)]
struct SpawnRuns<D: StateTreeDomain> {
    runs: Vec<SpawnRun<D>>,
    marker: PhantomData<fn() -> D>,
}

impl<D: StateTreeDomain> Default for SpawnRuns<D> {
    fn default() -> Self {
        Self {
            runs: Vec::new(),
            marker: PhantomData,
        }
    }
}

pub(crate) fn register<D: StateTreeDomain>(app: &mut App, schedule: InternedScheduleLabel) {
    crate::activity::register_activity::<D, SpawnActivity, _>(
        app,
        NAME.into(),
        |_context, arguments| {
            let definition: String = arguments.take("definition")?;
            let path = AssetPath::try_parse(&definition).map_err(|error| {
                crate::StateTreeActivityError::argument(
                    "definition",
                    format!("invalid state-tree definition asset path: {error}"),
                )
            })?;
            if path.path().as_os_str().is_empty() {
                return Err(crate::StateTreeActivityError::argument(
                    "definition",
                    "state-tree definition asset path cannot be empty",
                ));
            }
            if path.path().extension().and_then(|value| value.to_str()) != Some("scxml") {
                return Err(crate::StateTreeActivityError::argument(
                    "definition",
                    "state-tree definition asset path must end in .scxml",
                ));
            }
            Ok(SpawnActivity { definition })
        },
        crate::protocol::RegistrationOrigin::BuiltIn,
    );
    app.init_resource::<SpawnRuns<D>>().add_systems(
        schedule,
        (handle_lifecycle::<D>, monitor_runs::<D>)
            .chain()
            .in_set(StateTreeSet::HandleActivities),
    );
}

fn handle_lifecycle<D: StateTreeDomain>(
    mut lifecycle: MessageReader<StateTreeActivityLifecycle<D, SpawnActivity>>,
    asset_server: Res<AssetServer>,
    mut runs: ResMut<SpawnRuns<D>>,
    mut commands: Commands,
) {
    for message in lifecycle.read() {
        match message {
            StateTreeActivityLifecycle::Started { token, activity } => {
                let handle = asset_server.load::<StateTreeDefinition>(&activity.definition);
                runs.runs.push(SpawnRun {
                    token: *token,
                    definition: activity.definition.clone(),
                    state: SpawnRunState::Loading(handle),
                });
            }
            StateTreeActivityLifecycle::Cancelled { token } => {
                let mut index = 0;
                while index < runs.runs.len() {
                    if runs.runs[index].token != *token {
                        index += 1;
                        continue;
                    }
                    let run = runs.runs.swap_remove(index);
                    if let SpawnRunState::Running(child) = run.state {
                        commands.entity(child).despawn();
                    }
                }
            }
        }
    }
}

fn monitor_runs<D: StateTreeDomain>(
    mut commands: Commands,
    definitions: Res<Assets<StateTreeDefinition>>,
    children: Query<(&StateTreeInstance<D>, Option<&StateTreeLinkFailure<D>>)>,
    mut load_failures: MessageReader<AssetLoadFailedEvent<StateTreeDefinition>>,
    mut runs: ResMut<SpawnRuns<D>>,
    mut outcomes: StateTreeActivityOutcomeWriter<D, SpawnActivity>,
) {
    let failures: HashMap<_, _> = load_failures
        .read()
        .map(|failure| (failure.id, failure.error.to_string()))
        .collect();

    let mut index = 0;
    while index < runs.runs.len() {
        let outcome = match runs.runs[index].state {
            SpawnRunState::Loading(ref handle) => {
                if let Some(error) = failures.get(&handle.id()) {
                    Some(Err(format!(
                        "failed to load spawned state-tree definition `{}`: {error}",
                        runs.runs[index].definition
                    )))
                } else if definitions.contains(handle.id()) {
                    let child = commands
                        .spawn(StateTreeInstance::<D>::new(handle.clone()))
                        .id();
                    runs.runs[index].state = SpawnRunState::Running(child);
                    None
                } else {
                    None
                }
            }
            SpawnRunState::Running(child) => match children.get(child) {
                Ok((_, Some(failure))) => Some(Err(format!(
                    "spawned state tree `{}` failed to link: {}",
                    runs.runs[index].definition,
                    failure.error()
                ))),
                Ok((instance, None)) => match instance.status() {
                    InstanceStatus::Complete => Some(Ok(())),
                    InstanceStatus::Faulted(fault) => Some(Err(format!(
                        "spawned state tree `{}` faulted: {fault:?}",
                        runs.runs[index].definition
                    ))),
                    InstanceStatus::Pending | InstanceStatus::Running => None,
                },
                Err(_) => Some(Err(format!(
                    "spawned state tree `{}` entity disappeared unexpectedly",
                    runs.runs[index].definition
                ))),
            },
        };

        let Some(outcome) = outcome else {
            index += 1;
            continue;
        };
        let run = runs.runs.swap_remove(index);
        if let SpawnRunState::Running(child) = run.state {
            commands.entity(child).despawn();
        }
        match outcome {
            Ok(()) => {
                outcomes.complete(run.token, ());
            }
            Err(error) => {
                outcomes.fail(run.token, error);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bevy_app::{App, TaskPoolPlugin, Update};
    use bevy_asset::{AssetPlugin, Assets};
    use bevy_time::{Time, TimePlugin};

    use super::*;
    use crate::{
        StateTreeActivityDefinition, StateTreeArgument, StateTreeBuilder, StateTreeConfig,
        StateTreeData, StateTreeTransition,
    };

    struct TestDomain;

    impl StateTreeDomain for TestDomain {
        const AUTHORED_NAME: &'static str = "test";

        fn configure(_config: &mut StateTreeConfig<'_, Self>) {}
    }

    struct ReservedNameDomain;

    impl StateTreeDomain for ReservedNameDomain {
        const AUTHORED_NAME: &'static str = "reserved-name";

        fn configure(config: &mut StateTreeConfig<'_, Self>) {
            config.activity::<SpawnActivity, _>(NAME, |_context, arguments| {
                Ok(SpawnActivity {
                    definition: arguments.take("definition")?,
                })
            });
        }
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
            crate::StateTreePlugin::<TestDomain>::new(Update),
        ));
        app
    }

    fn parent_definition(path: &str, cancel_after: Option<Duration>) -> StateTreeDefinition {
        let mut builder = StateTreeBuilder::new("spawn-parent");
        builder.luau_datamodel([]).unwrap();
        let root = builder.compound_root("root").unwrap();
        let active = builder.atomic("active", root).unwrap();
        let succeeded = builder.final_state("succeeded", root).unwrap();
        let failed = builder.final_state("failed", root).unwrap();
        builder.initial(root, active).unwrap();
        builder
            .add_activity(
                active,
                StateTreeActivityDefinition::new("child", NAME)
                    .arguments([StateTreeArgument::new("definition", format!("{path:?}"))]),
            )
            .unwrap();
        builder
            .add_transition(
                active,
                StateTreeTransition::on("done.activity.child").target(succeeded),
            )
            .unwrap();
        builder
            .add_transition(
                active,
                StateTreeTransition::on("error.activity.child")
                    .condition("#_event.data > 0")
                    .target(failed),
            )
            .unwrap();
        if let Some(duration) = cancel_after {
            let cancelled = builder.atomic("cancelled", root).unwrap();
            builder
                .add_transition(
                    active,
                    StateTreeTransition::after(duration).target(cancelled),
                )
                .unwrap();
        }
        builder.build().unwrap()
    }

    fn running_child_definition() -> StateTreeDefinition {
        let mut builder = StateTreeBuilder::new("running-child");
        let root = builder.compound_root("root").unwrap();
        let active = builder.atomic("active", root).unwrap();
        builder.initial(root, active).unwrap();
        builder.build().unwrap()
    }

    fn complete_child_definition() -> StateTreeDefinition {
        let mut builder = StateTreeBuilder::new("complete-child");
        let root = builder.compound_root("root").unwrap();
        let done = builder.final_state("done", root).unwrap();
        builder.initial(root, done).unwrap();
        builder.build().unwrap()
    }

    fn faulted_child_definition() -> StateTreeDefinition {
        let mut builder = StateTreeBuilder::new("faulted-child");
        builder
            .luau_datamodel([StateTreeData::new("value").with_initializer("missing.value")])
            .unwrap();
        let root = builder.compound_root("root").unwrap();
        let active = builder.atomic("active", root).unwrap();
        builder.initial(root, active).unwrap();
        builder.build().unwrap()
    }

    fn unlinked_child_definition() -> StateTreeDefinition {
        let mut builder = StateTreeBuilder::new("unlinked-child");
        let root = builder.compound_root("root").unwrap();
        let active = builder.atomic("active", root).unwrap();
        builder.initial(root, active).unwrap();
        builder
            .add_activity(
                active,
                StateTreeActivityDefinition::new("unknown", "not.registered"),
            )
            .unwrap();
        builder.build().unwrap()
    }

    fn spawn_parent(app: &mut App, path: &str, cancel_after: Option<Duration>) -> Entity {
        let definition = parent_definition(path, cancel_after);
        let handle = app
            .world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .add(definition);
        app.world_mut()
            .spawn(StateTreeInstance::<TestDomain>::new(handle))
            .id()
    }

    fn insert_for_run(app: &mut App, definition: StateTreeDefinition) {
        let handle = match &app
            .world()
            .resource::<SpawnRuns<TestDomain>>()
            .runs
            .first()
            .expect("spawn run should be tracked")
            .state
        {
            SpawnRunState::Loading(handle) => handle.clone(),
            SpawnRunState::Running(_) => panic!("spawn run should still be loading"),
        };
        app.world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .insert(handle.id(), definition)
            .unwrap();
    }

    fn instance_count(app: &mut App) -> usize {
        app.world_mut()
            .query::<&StateTreeInstance<TestDomain>>()
            .iter(app.world())
            .count()
    }

    fn update_state_trees(app: &mut App) {
        app.world_mut().run_schedule(Update);
    }

    #[test]
    fn waits_for_definition_then_spawns_independent_child() {
        let mut app = app();
        let parent = spawn_parent(&mut app, "loaded.scxml", None);

        app.update();
        assert_eq!(instance_count(&mut app), 1);
        assert_eq!(
            app.world().resource::<SpawnRuns<TestDomain>>().runs.len(),
            1
        );

        insert_for_run(&mut app, running_child_definition());
        update_state_trees(&mut app);
        assert_eq!(instance_count(&mut app), 2);
        update_state_trees(&mut app);

        let mut children = app
            .world_mut()
            .query::<(Entity, &StateTreeInstance<TestDomain>)>();
        let (child, instance) = children
            .iter(app.world())
            .find(|(entity, _)| *entity != parent)
            .expect("a separate child state-tree entity should exist");
        assert_eq!(instance.status(), &InstanceStatus::Running);
        assert!(
            app.world()
                .get_entity(child)
                .unwrap()
                .get::<bevy_ecs::hierarchy::ChildOf>()
                .is_none()
        );
    }

    #[test]
    fn cancellation_while_loading_removes_the_run_without_spawning() {
        let mut app = app();
        let parent = spawn_parent(
            &mut app,
            "spawn-cancel.scxml",
            Some(Duration::from_millis(1)),
        );
        app.update();

        app.world_mut()
            .resource_mut::<Time>()
            .advance_by(Duration::from_millis(1));
        app.update();

        assert_eq!(instance_count(&mut app), 1);
        assert!(
            app.world()
                .resource::<SpawnRuns<TestDomain>>()
                .runs
                .is_empty()
        );
        assert_eq!(
            app.world()
                .get::<StateTreeInstance<TestDomain>>(parent)
                .unwrap()
                .active_state_names()
                .last(),
            Some("cancelled")
        );
    }

    #[test]
    fn invalid_definition_paths_fault_the_parent_activity() {
        for path in ["", "://child.scxml", "child.txt"] {
            let mut app = app();
            let parent = spawn_parent(&mut app, path, None);

            app.update();

            assert!(matches!(
                app.world()
                    .get::<StateTreeInstance<TestDomain>>(parent)
                    .unwrap()
                    .status(),
                InstanceStatus::Faulted(crate::RuntimeFault::Activity {
                    activity,
                    argument: Some(argument),
                    ..
                }) if activity.as_ref() == NAME && argument.as_ref() == "definition"
            ));
            assert!(
                app.world()
                    .resource::<SpawnRuns<TestDomain>>()
                    .runs
                    .is_empty()
            );
        }
    }

    #[test]
    fn cancellation_despawns_a_running_child() {
        let mut app = app();
        let parent = spawn_parent(&mut app, "loaded.scxml", Some(Duration::from_millis(1)));
        app.update();
        insert_for_run(&mut app, running_child_definition());
        update_state_trees(&mut app);
        assert_eq!(instance_count(&mut app), 2);

        app.world_mut()
            .resource_mut::<Time>()
            .advance_by(Duration::from_millis(1));
        update_state_trees(&mut app);

        assert_eq!(instance_count(&mut app), 1);
        assert!(
            app.world()
                .resource::<SpawnRuns<TestDomain>>()
                .runs
                .is_empty()
        );
        assert_eq!(
            app.world()
                .get::<StateTreeInstance<TestDomain>>(parent)
                .unwrap()
                .active_state_names()
                .last(),
            Some("cancelled")
        );
    }

    #[test]
    fn completed_child_completes_parent_activity_and_is_despawned() {
        let mut app = app();
        let parent = spawn_parent(&mut app, "loaded.scxml", None);
        app.update();
        insert_for_run(&mut app, complete_child_definition());

        for _ in 0..3 {
            update_state_trees(&mut app);
        }

        let instance = app
            .world()
            .get::<StateTreeInstance<TestDomain>>(parent)
            .unwrap();
        assert_eq!(instance.status(), &InstanceStatus::Complete);
        assert_eq!(instance.active_state_names().last(), Some("succeeded"));
        assert_eq!(instance_count(&mut app), 1);
    }

    fn assert_child_failure(definition: StateTreeDefinition, path: &str) {
        let mut app = app();
        let parent = spawn_parent(&mut app, path, None);
        app.update();
        insert_for_run(&mut app, definition);

        for _ in 0..3 {
            update_state_trees(&mut app);
        }

        let instance = app
            .world()
            .get::<StateTreeInstance<TestDomain>>(parent)
            .unwrap();
        assert_eq!(instance.status(), &InstanceStatus::Complete);
        assert_eq!(instance.active_state_names().last(), Some("failed"));
        assert_eq!(instance_count(&mut app), 1);
    }

    #[test]
    fn faulted_child_fails_parent_activity_with_string_data() {
        assert_child_failure(faulted_child_definition(), "loaded.scxml");
    }

    #[test]
    fn child_link_failure_fails_parent_activity_with_string_data() {
        assert_child_failure(unlinked_child_definition(), "loaded.scxml");
    }

    #[test]
    fn unexpectedly_missing_child_fails_parent_activity() {
        let mut app = app();
        let parent = spawn_parent(&mut app, "loaded.scxml", None);
        app.update();
        insert_for_run(&mut app, running_child_definition());
        update_state_trees(&mut app);

        let child = match app
            .world()
            .resource::<SpawnRuns<TestDomain>>()
            .runs
            .first()
            .map(|run| &run.state)
        {
            Some(SpawnRunState::Running(child)) => *child,
            _ => panic!("child should be running"),
        };
        app.world_mut().despawn(child);
        update_state_trees(&mut app);
        update_state_trees(&mut app);

        let instance = app
            .world()
            .get::<StateTreeInstance<TestDomain>>(parent)
            .unwrap();
        assert_eq!(instance.status(), &InstanceStatus::Complete);
        assert_eq!(instance.active_state_names().last(), Some("failed"));
    }

    #[test]
    fn definition_load_failure_fails_parent_activity_with_string_data() {
        let mut app = app();
        let parent = spawn_parent(&mut app, "does-not-exist.scxml", None);

        for _ in 0..1_000 {
            app.update();
            std::thread::yield_now();
            if app
                .world()
                .get::<StateTreeInstance<TestDomain>>(parent)
                .is_some_and(|instance| instance.status() == &InstanceStatus::Complete)
            {
                break;
            }
        }

        let instance = app
            .world()
            .get::<StateTreeInstance<TestDomain>>(parent)
            .unwrap();
        assert_eq!(instance.status(), &InstanceStatus::Complete);
        assert_eq!(instance.active_state_names().last(), Some("failed"));
        assert_eq!(instance_count(&mut app), 1);
    }

    #[test]
    #[should_panic(
        expected = "state-tree activity `bst.spawn` is reserved for the built-in state-tree spawner"
    )]
    fn domain_cannot_replace_builtin_spawn_activity() {
        let mut app = App::new();
        app.add_plugins((
            TaskPoolPlugin::default(),
            TimePlugin,
            AssetPlugin::default(),
            crate::StateTreePlugin::<ReservedNameDomain>::new(Update),
        ));
    }
}
