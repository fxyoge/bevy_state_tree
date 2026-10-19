mod support;

use bevy_state_tree::persistence::Checkpoint;
use divan::{Bencher, counter::ItemsCount};
use support::{BenchmarkApp, Executor};

fn main() {
    divan::main();
}

fn benchmark(instances: usize) -> BenchmarkApp {
    let mut benchmark = BenchmarkApp::empty(support::activity_definition(), Executor::Parallel);
    benchmark.register_benchmark_activity();
    benchmark.spawn_instances(instances);
    benchmark.run_schedule();
    benchmark
}

#[divan::bench(args = [1_000, 10_000], sample_count = 10)]
fn capture_with_consumer_tokens(bencher: Bencher<'_, '_>, instances: usize) {
    let mut benchmark = benchmark(instances);
    bencher.counter(ItemsCount::new(instances)).bench_local(|| {
        divan::black_box(Checkpoint::capture(benchmark.app.world_mut()).unwrap());
    });
}

#[divan::bench(args = [1_000, 10_000], sample_count = 10)]
fn restore_with_consumer_tokens(bencher: Bencher<'_, '_>, instances: usize) {
    let mut benchmark = benchmark(instances);
    let checkpoint = Checkpoint::capture(benchmark.app.world_mut()).unwrap();
    bencher.counter(ItemsCount::new(instances)).bench_local(|| {
        checkpoint.restore(benchmark.app.world_mut()).unwrap();
    });
}
