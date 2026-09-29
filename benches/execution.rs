use std::cell::RefCell;

use bevy_app::{App, TaskPoolPlugin};
use bevy_asset::{AssetPlugin, Assets};
use bevy_ecs::{event::Event, schedule::ScheduleLabel, system::RunSystemOnce};
use bevy_state_tree::{
    BroadcastStateTreeEventWriter, StateTreeDefinition, StateTreeEvents, StateTreeInstance,
    StateTreePlugin, scxml,
};
use divan::{Bencher, counter::ItemsCount};

const TOGGLE: &str = include_str!("../tests/scxml/benchmark_toggle.scxml");
const LUAU: &str = include_str!("../tests/scxml/benchmark_luau.scxml");

fn main() {
    divan::main();
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, ScheduleLabel)]
struct AiUpdate;

struct Simulation;

#[derive(Event)]
struct Tick;

#[derive(StateTreeEvents)]
#[state_tree(domain = Simulation)]
enum Events {
    #[state_tree(name = "tick", broadcast)]
    Tick(Tick),
}

fn queue_tick(mut events: BroadcastStateTreeEventWriter<Simulation>) {
    events.write(Tick);
}

fn benchmark_app(source_name: &str, source: &str, instances: usize) -> App {
    let document = scxml::parse(source_name, source).unwrap();
    let definition = scxml::compile(&document).unwrap();

    let mut app = App::new();
    app.add_plugins((TaskPoolPlugin::default(), AssetPlugin::default()))
        .init_schedule(AiUpdate)
        .add_plugins(StateTreePlugin::<Simulation>::new(AiUpdate));
    let handle = app
        .world_mut()
        .resource_mut::<Assets<StateTreeDefinition>>()
        .add(definition);
    app.world_mut()
        .spawn_batch((0..instances).map(|_| StateTreeInstance::<Simulation>::new(handle.clone())));

    // Link and initialize every instance before measuring event processing.
    app.world_mut().run_schedule(AiUpdate);
    app
}

fn process_population(
    bencher: Bencher<'_, '_>,
    instances: usize,
    source_name: &'static str,
    source: &'static str,
) {
    let app = RefCell::new(benchmark_app(source_name, source, instances));

    bencher
        .counter(ItemsCount::new(instances))
        .with_inputs(|| {
            app.borrow_mut()
                .world_mut()
                .run_system_once(queue_tick)
                .unwrap();
        })
        .bench_local_values(|()| {
            app.borrow_mut().world_mut().run_schedule(AiUpdate);
        });
}

#[divan::bench(args = [1_000, 100_000, 1_000_000], sample_count = 20)]
fn process_toggle_population(bencher: Bencher<'_, '_>, instances: usize) {
    process_population(bencher, instances, "benchmark_toggle.scxml", TOGGLE);
}

#[divan::bench(args = [1_000, 100_000, 1_000_000], sample_count = 10)]
fn process_luau_population(bencher: Bencher<'_, '_>, instances: usize) {
    process_population(bencher, instances, "benchmark_luau.scxml", LUAU);
}
