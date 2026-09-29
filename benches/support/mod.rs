#![allow(dead_code)]

use bevy_app::{App, TaskPoolPlugin};
use bevy_asset::{AssetPlugin, Assets};
use bevy_ecs::{
    event::{EntityEvent, Event},
    message::{Message, Messages},
    prelude::{Entity, Query, ResMut, Resource, With},
    schedule::{IntoScheduleConfigs, ScheduleLabel, SingleThreadedExecutor},
    system::{Res, RunSystemOnce},
};
use bevy_state_tree::{
    BroadcastStateTreeEventWriter, EntityStateTreeEventWriter, StateTreeAppExt, StateTreeBuilder,
    StateTreeDefinition, StateTreeEvents, StateTreeInstance, StateTreePlugin, StateTreeSet, scxml,
};

const TOGGLE: &str = include_str!("../../tests/scxml/benchmark_toggle.scxml");
const LUAU: &str = include_str!("../../tests/scxml/benchmark_luau.scxml");
const PARALLEL: &str = include_str!("../../tests/scxml/benchmark_parallel.scxml");
const EFFECT: &str = r#"
<scxml xmlns="http://www.w3.org/2005/07/scxml"
       xmlns:bst="urn:bevy-state-tree" datamodel="luau">
  <datamodel><data id="amount" expr="1"/></datamodel>
  <state id="left">
    <transition event="tick" target="right">
      <bst:effect name="benchmark"><bst:arg name="amount" expr="amount"/></bst:effect>
    </transition>
  </state>
  <state id="right">
    <transition event="tick" target="left">
      <bst:effect name="benchmark"><bst:arg name="amount" expr="amount"/></bst:effect>
    </transition>
  </state>
</scxml>
"#;

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
    pub fn empty(definition: StateTreeDefinition, executor: Executor) -> Self {
        let mut app = App::new();
        if matches!(executor, Executor::Parallel) {
            app.add_plugins(TaskPoolPlugin::default());
        }
        app.add_plugins(AssetPlugin::default())
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

        let handle = app
            .world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .add(definition);
        app.insert_resource(TargetCount(0));
        Self {
            app,
            definition: handle,
        }
    }

    pub fn pending(definition: StateTreeDefinition, instances: usize, executor: Executor) -> Self {
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

    pub fn initialized(
        definition: StateTreeDefinition,
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

pub fn toggle_definition() -> StateTreeDefinition {
    compile("benchmark_toggle.scxml", TOGGLE)
}

pub fn luau_definition() -> StateTreeDefinition {
    compile("benchmark_luau.scxml", LUAU)
}

pub fn parallel_definition() -> StateTreeDefinition {
    compile("benchmark_parallel.scxml", PARALLEL)
}

pub fn effect_definition() -> StateTreeDefinition {
    compile("benchmark_effect.scxml", EFFECT)
}

pub fn entity_toggle_definition() -> StateTreeDefinition {
    toggle_builder("entity-toggle", "tick.entity")
}

pub fn deep_definition(depth: usize) -> StateTreeDefinition {
    assert!(depth > 0);
    let mut builder = StateTreeBuilder::new(format!("depth-{depth}"));
    let root = builder.compound_root("root").unwrap();
    let left = add_branch(&mut builder, root, "left", depth);
    let right = add_branch(&mut builder, root, "right", depth);
    builder.initial(root, left.0).unwrap();
    builder.transition(left.1, "tick", right.1).unwrap();
    builder.transition(right.1, "tick", left.1).unwrap();
    builder.build().unwrap()
}

pub fn wide_parallel_definition(regions: usize) -> StateTreeDefinition {
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
        builder.transition(left, "tick", right).unwrap();
        builder.transition(right, "tick", left).unwrap();
    }
    builder.build().unwrap()
}

pub fn transition_scan_definition(candidates: usize) -> StateTreeDefinition {
    let mut builder = StateTreeBuilder::new(format!("transition-scan-{candidates}"));
    let root = builder.compound_root("root").unwrap();
    let left = builder.atomic("left", root).unwrap();
    let right = builder.atomic("right", root).unwrap();
    builder.initial(root, left).unwrap();
    for _ in 0..candidates {
        builder.transition(left, "ignored", right).unwrap();
        builder.transition(right, "ignored", left).unwrap();
    }
    builder.transition(left, "tick", right).unwrap();
    builder.transition(right, "tick", left).unwrap();
    builder.build().unwrap()
}

fn toggle_builder(name: &str, event: &str) -> StateTreeDefinition {
    let mut builder = StateTreeBuilder::new(name);
    let root = builder.compound_root("root").unwrap();
    let left = builder.atomic("left", root).unwrap();
    let right = builder.atomic("right", root).unwrap();
    builder.initial(root, left).unwrap();
    builder.transition(left, event, right).unwrap();
    builder.transition(right, event, left).unwrap();
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

fn compile(name: &str, source: &str) -> StateTreeDefinition {
    let document = scxml::parse(name, source).unwrap();
    scxml::compile(&document).unwrap()
}
