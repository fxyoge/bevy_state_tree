#![allow(dead_code)]

use std::time::Duration;

use bevy_app::{App, TaskPoolPlugin};
use bevy_asset::{AssetPlugin, AssetServer, Assets, Handle, LoadState};
use bevy_ecs::{
    event::{EntityEvent, Event},
    message::{Message, MessageReader, Messages},
    prelude::{Entity, Query, ResMut, Resource, With},
    schedule::{IntoScheduleConfigs, ScheduleLabel, SingleThreadedExecutor},
    system::{Res, RunSystemOnce},
};
use bevy_state_tree::{
    BroadcastStateTreeEventWriter, EntityStateTreeEventWriter, StateTreeActivity,
    StateTreeActivityDefinition, StateTreeActivityLifecycle, StateTreeActivityOutcomeWriter,
    StateTreeActivityToken, StateTreeAppExt, StateTreeBuilder, StateTreeDefinition,
    StateTreeEvents, StateTreeInstance, StateTreePlugin, StateTreeSet, StateTreeTransition,
};
use bevy_tasks::{AsyncComputeTaskPool, IoTaskPool, TaskPoolBuilder};
use bevy_time::{Time, TimePlugin};

const TOGGLE: &str = "tests/scxml/benchmark_toggle.scxml";
const LUAU: &str = "tests/scxml/benchmark_luau.scxml";
const PARALLEL: &str = "tests/scxml/benchmark_parallel.scxml";
const EFFECT: &str = "benches/assets/benchmark_effect.scxml";

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, ScheduleLabel)]
pub struct AiUpdate;

pub struct Simulation;

#[derive(Event)]
pub struct Tick;

#[derive(EntityEvent)]
pub struct EntityTick {
    entity: Entity,
}

#[derive(Event)]
pub struct Ignored;

#[derive(Message)]
struct BenchmarkEffect;

struct BenchmarkActivity;

impl StateTreeActivity for BenchmarkActivity {
    type Output = ();
    type Error = ();
}

#[derive(Resource, Default)]
struct BenchmarkActivityTokens(Vec<StateTreeActivityToken<Simulation, BenchmarkActivity>>);

#[derive(StateTreeEvents)]
#[state_tree(domain = Simulation)]
pub enum Events {
    #[state_tree(name = "tick", broadcast)]
    Tick(Tick),
    #[state_tree(name = "tick.entity")]
    EntityTick(EntityTick),
    #[state_tree(name = "ignored", broadcast)]
    Ignored(Ignored),
}

#[derive(Clone, Copy)]
pub enum Executor {
    Parallel,
    SingleThreaded,
}

pub enum BenchmarkDefinition {
    Asset(&'static str),
    Generated(StateTreeDefinition),
}

#[derive(Resource)]
struct TargetCount(usize);

#[derive(Resource)]
enum QueuedEvent {
    None,
    Broadcast,
    Entity(usize),
}

pub struct BenchmarkApp {
    pub app: App,
    definition: bevy_asset::Handle<StateTreeDefinition>,
}

impl BenchmarkApp {
    pub fn empty(definition: BenchmarkDefinition, executor: Executor) -> Self {
        let mut app = App::new();
        if matches!(executor, Executor::Parallel) {
            app.add_plugins(TaskPoolPlugin::default());
        } else {
            IoTaskPool::get_or_init(|| TaskPoolBuilder::new().num_threads(1).build());
            AsyncComputeTaskPool::get_or_init(|| TaskPoolBuilder::new().num_threads(1).build());
        }
        app.add_plugins((
            TimePlugin,
            AssetPlugin {
                file_path: env!("CARGO_MANIFEST_DIR").into(),
                watch_for_changes_override: Some(false),
                use_asset_processor_override: Some(false),
                ..Default::default()
            },
        ))
        .init_schedule(AiUpdate)
        .add_plugins(StateTreePlugin::<Simulation>::new(AiUpdate))
        .insert_resource(QueuedEvent::None)
        .add_systems(
            AiUpdate,
            queue_configured_event.in_set(StateTreeSet::QueueEvents),
        );
        if matches!(executor, Executor::SingleThreaded) {
            app.edit_schedule(AiUpdate, |schedule| {
                schedule.set_executor(SingleThreadedExecutor::new());
            });
        }

        let handle = match definition {
            BenchmarkDefinition::Asset(path) => load_definition(&mut app, path),
            BenchmarkDefinition::Generated(definition) => app
                .world_mut()
                .resource_mut::<Assets<StateTreeDefinition>>()
                .add(definition),
        };
        app.insert_resource(TargetCount(0));
        Self {
            app,
            definition: handle,
        }
    }

    pub fn pending(definition: BenchmarkDefinition, instances: usize, executor: Executor) -> Self {
        let mut benchmark = Self::empty(definition, executor);
        benchmark.spawn_instances(instances);
        benchmark
    }

    pub fn spawn_instances(&mut self, instances: usize) {
        for _ in 0..instances {
            self.app
                .world_mut()
                .spawn(StateTreeInstance::<Simulation>::new(
                    self.definition.clone(),
                ));
        }
    }

    pub fn register_benchmark_effect(&mut self) {
        self.app
            .register_state_tree_effect::<Simulation, BenchmarkEffect, _>(
                "benchmark",
                |_context, arguments| {
                    let _: i64 = arguments.take("amount")?;
                    Ok(BenchmarkEffect)
                },
            )
            .add_systems(
                AiUpdate,
                clear_benchmark_effects.in_set(StateTreeSet::HandleEffects),
            );
    }

    pub fn register_benchmark_activity(&mut self) {
        self.app
            .register_state_tree_activity::<Simulation, BenchmarkActivity, _>(
                "benchmark",
                |_context, _arguments| Ok(BenchmarkActivity),
            )
            .init_resource::<BenchmarkActivityTokens>()
            .add_systems(
                AiUpdate,
                collect_benchmark_activities.in_set(StateTreeSet::HandleActivities),
            );
    }

    pub fn queue_activity_outcomes(&mut self) {
        self.app
            .world_mut()
            .run_system_once(complete_benchmark_activities)
            .expect("benchmark activity outcome system should run");
    }

    pub fn initialized(
        definition: BenchmarkDefinition,
        instances: usize,
        executor: Executor,
    ) -> Self {
        let mut benchmark = Self::pending(definition, instances, executor);
        benchmark.run_schedule();
        benchmark
    }

    pub fn run_schedule(&mut self) {
        self.app.world_mut().run_schedule(AiUpdate);
    }

    pub fn advance_time(&mut self, duration: Duration) {
        self.app
            .world_mut()
            .resource_mut::<Time>()
            .advance_by(duration);
    }

    pub fn queue_broadcast(&mut self) {
        self.app
            .world_mut()
            .run_system_once(queue_broadcast_tick)
            .expect("benchmark event system should run");
    }

    pub fn enable_broadcast_events(&mut self) {
        self.app.insert_resource(QueuedEvent::Broadcast);
    }

    pub fn enable_entity_events(&mut self, count: usize) {
        self.app.insert_resource(QueuedEvent::Entity(count));
    }

    pub fn queue_entity_events(&mut self, count: usize) {
        self.app.world_mut().resource_mut::<TargetCount>().0 = count;
        self.app
            .world_mut()
            .run_system_once(queue_entity_ticks)
            .expect("benchmark event system should run");
    }
}

fn load_definition(app: &mut App, path: &'static str) -> Handle<StateTreeDefinition> {
    let handle = app
        .world()
        .resource::<AssetServer>()
        .load::<StateTreeDefinition>(path);
    for _ in 0..1_000 {
        app.update();
        match app
            .world()
            .resource::<AssetServer>()
            .load_state(handle.id())
        {
            LoadState::Loaded => return handle,
            LoadState::Failed(error) => panic!("failed to load benchmark definition: {error}"),
            LoadState::NotLoaded | LoadState::Loading => std::thread::yield_now(),
        }
    }
    panic!("timed out loading benchmark definition `{path}`");
}

fn queue_broadcast_tick(mut events: BroadcastStateTreeEventWriter<Simulation>) {
    events.write(Tick);
}

fn queue_entity_ticks(
    instances: Query<Entity, With<StateTreeInstance<Simulation>>>,
    count: Res<TargetCount>,
    mut events: EntityStateTreeEventWriter<Simulation>,
) {
    let mut queued = 0;
    for entity in instances.iter().take(count.0) {
        assert!(events.write(EntityTick { entity }));
        queued += 1;
    }
    assert_eq!(queued, count.0);
}

fn queue_configured_event(
    event: Res<QueuedEvent>,
    instances: Query<Entity, With<StateTreeInstance<Simulation>>>,
    mut entity_events: EntityStateTreeEventWriter<Simulation>,
    mut broadcast_events: BroadcastStateTreeEventWriter<Simulation>,
) {
    match *event {
        QueuedEvent::None => {}
        QueuedEvent::Broadcast => broadcast_events.write(Tick),
        QueuedEvent::Entity(count) => {
            let mut queued = 0;
            for entity in instances.iter().take(count) {
                assert!(entity_events.write(EntityTick { entity }));
                queued += 1;
            }
            assert_eq!(queued, count);
        }
    }
}

fn clear_benchmark_effects(mut effects: ResMut<Messages<BenchmarkEffect>>) {
    effects.clear();
}

fn collect_benchmark_activities(
    mut messages: MessageReader<StateTreeActivityLifecycle<Simulation, BenchmarkActivity>>,
    mut tokens: ResMut<BenchmarkActivityTokens>,
) {
    for message in messages.read() {
        if let StateTreeActivityLifecycle::Started { token, .. } = message {
            tokens.0.push(*token);
        }
    }
}

fn complete_benchmark_activities(
    tokens: Res<BenchmarkActivityTokens>,
    mut outcomes: StateTreeActivityOutcomeWriter<Simulation, BenchmarkActivity>,
) {
    for token in &tokens.0 {
        assert!(outcomes.complete(*token, ()));
    }
}

pub fn toggle_definition() -> BenchmarkDefinition {
    BenchmarkDefinition::Asset(TOGGLE)
}

pub fn luau_definition() -> BenchmarkDefinition {
    BenchmarkDefinition::Asset(LUAU)
}

pub fn parallel_definition() -> BenchmarkDefinition {
    BenchmarkDefinition::Asset(PARALLEL)
}

pub fn effect_definition() -> BenchmarkDefinition {
    BenchmarkDefinition::Asset(EFFECT)
}

pub fn activity_definition() -> BenchmarkDefinition {
    let mut builder = StateTreeBuilder::new("activity");
    let root = builder.compound_root("root").unwrap();
    let active = builder.atomic("active", root).unwrap();
    let done = builder.atomic("done", root).unwrap();
    builder.initial(root, active).unwrap();
    builder
        .add_activity(active, StateTreeActivityDefinition::new("job", "benchmark"))
        .unwrap();
    builder
        .add_transition(
            active,
            StateTreeTransition::on("done.activity.job").target(done),
        )
        .unwrap();
    BenchmarkDefinition::Generated(builder.build().unwrap())
}

pub fn entity_toggle_definition() -> BenchmarkDefinition {
    BenchmarkDefinition::Generated(toggle_builder("entity-toggle", "tick.entity"))
}

pub fn deep_definition(depth: usize) -> BenchmarkDefinition {
    assert!(depth > 0);
    let mut builder = StateTreeBuilder::new(format!("depth-{depth}"));
    let root = builder.compound_root("root").unwrap();
    let left = add_branch(&mut builder, root, "left", depth);
    let right = add_branch(&mut builder, root, "right", depth);
    builder.initial(root, left.0).unwrap();
    builder
        .add_transition(left.1, StateTreeTransition::on("tick").target(right.1))
        .unwrap();
    builder
        .add_transition(right.1, StateTreeTransition::on("tick").target(left.1))
        .unwrap();
    BenchmarkDefinition::Generated(builder.build().unwrap())
}

pub fn wide_parallel_definition(regions: usize) -> BenchmarkDefinition {
    assert!(regions > 0);
    let mut builder = StateTreeBuilder::new(format!("parallel-{regions}"));
    let root = builder.compound_root("root").unwrap();
    let parallel = builder.parallel("active", root).unwrap();
    builder.initial(root, parallel).unwrap();
    for region_index in 0..regions {
        let region = builder
            .compound(format!("region-{region_index}"), parallel)
            .unwrap();
        let left = builder
            .atomic(format!("region-{region_index}-left"), region)
            .unwrap();
        let right = builder
            .atomic(format!("region-{region_index}-right"), region)
            .unwrap();
        builder.initial(region, left).unwrap();
        builder
            .add_transition(left, StateTreeTransition::on("tick").target(right))
            .unwrap();
        builder
            .add_transition(right, StateTreeTransition::on("tick").target(left))
            .unwrap();
    }
    BenchmarkDefinition::Generated(builder.build().unwrap())
}

pub fn transition_scan_definition(candidates: usize) -> BenchmarkDefinition {
    let mut builder = StateTreeBuilder::new(format!("transition-scan-{candidates}"));
    let root = builder.compound_root("root").unwrap();
    let left = builder.atomic("left", root).unwrap();
    let right = builder.atomic("right", root).unwrap();
    builder.initial(root, left).unwrap();
    for _ in 0..candidates {
        builder
            .add_transition(left, StateTreeTransition::on("ignored").target(right))
            .unwrap();
        builder
            .add_transition(right, StateTreeTransition::on("ignored").target(left))
            .unwrap();
    }
    builder
        .add_transition(left, StateTreeTransition::on("tick").target(right))
        .unwrap();
    builder
        .add_transition(right, StateTreeTransition::on("tick").target(left))
        .unwrap();
    BenchmarkDefinition::Generated(builder.build().unwrap())
}

pub fn timer_definition(duration: Duration) -> BenchmarkDefinition {
    let mut builder = StateTreeBuilder::new("timer");
    let root = builder.compound_root("root").unwrap();
    let waiting = builder.atomic("waiting", root).unwrap();
    let elapsed = builder.atomic("elapsed", root).unwrap();
    builder.initial(root, waiting).unwrap();
    builder
        .add_transition(
            waiting,
            StateTreeTransition::after(duration).target(elapsed),
        )
        .unwrap();
    BenchmarkDefinition::Generated(builder.build().unwrap())
}

fn toggle_builder(name: &str, event: &str) -> StateTreeDefinition {
    let mut builder = StateTreeBuilder::new(name);
    let root = builder.compound_root("root").unwrap();
    let left = builder.atomic("left", root).unwrap();
    let right = builder.atomic("right", root).unwrap();
    builder.initial(root, left).unwrap();
    builder
        .add_transition(left, StateTreeTransition::on(event).target(right))
        .unwrap();
    builder
        .add_transition(right, StateTreeTransition::on(event).target(left))
        .unwrap();
    builder.build().unwrap()
}

fn add_branch(
    builder: &mut StateTreeBuilder,
    root: bevy_state_tree::StateId,
    prefix: &str,
    depth: usize,
) -> (bevy_state_tree::StateId, bevy_state_tree::StateId) {
    let top = if depth == 1 {
        builder.atomic(format!("{prefix}-leaf"), root).unwrap()
    } else {
        builder.compound(format!("{prefix}-0"), root).unwrap()
    };
    let mut parent = top;
    for level in 1..depth {
        let child = if level + 1 == depth {
            builder.atomic(format!("{prefix}-leaf"), parent).unwrap()
        } else {
            builder
                .compound(format!("{prefix}-{level}"), parent)
                .unwrap()
        };
        builder.initial(parent, child).unwrap();
        parent = child;
    }
    (top, parent)
}
