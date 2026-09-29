mod support;

use divan::{AllocProfiler, Bencher, counter::ItemsCount};
use support::{BenchmarkApp, BenchmarkDefinition, Executor};

#[global_allocator]
static ALLOC: AllocProfiler = AllocProfiler::system();

fn main() {
    divan::main();
}

fn profile_processing(
    bencher: Bencher<'_, '_>,
    instances: usize,
    definition: impl Fn() -> BenchmarkDefinition,
) {
    bencher
        .counter(ItemsCount::new(instances))
        .with_inputs(|| {
            let mut benchmark =
                BenchmarkApp::initialized(definition(), instances, Executor::SingleThreaded);
            benchmark.queue_broadcast();
            benchmark
        })
        .bench_local_values(|mut benchmark| {
            benchmark.run_schedule();
            divan::black_box(benchmark)
        });
}

fn profile_effect_processing(bencher: Bencher<'_, '_>, instances: usize) {
    bencher
        .counter(ItemsCount::new(instances))
        .with_inputs(|| {
            let mut benchmark =
                BenchmarkApp::empty(support::effect_definition(), Executor::SingleThreaded);
            benchmark.register_benchmark_effect();
            benchmark.spawn_instances(instances);
            benchmark.run_schedule();
            benchmark.queue_broadcast();
            benchmark
        })
        .bench_local_values(|mut benchmark| {
            benchmark.run_schedule();
            divan::black_box(benchmark)
        });
}

fn profile_initialization(
    bencher: Bencher<'_, '_>,
    instances: usize,
    definition: impl Fn() -> BenchmarkDefinition,
) {
    bencher
        .counter(ItemsCount::new(instances))
        .with_inputs(|| BenchmarkApp::pending(definition(), instances, Executor::SingleThreaded))
        .bench_local_values(|mut benchmark| {
            benchmark.run_schedule();
            divan::black_box(benchmark)
        });
}

fn profile_spawn_and_initialization(
    bencher: Bencher<'_, '_>,
    instances: usize,
    definition: impl Fn() -> BenchmarkDefinition,
) {
    bencher
        .counter(ItemsCount::new(instances))
        .with_inputs(|| {
            let mut benchmark = BenchmarkApp::empty(definition(), Executor::SingleThreaded);
            benchmark.run_schedule();
            benchmark
        })
        .bench_local_values(|mut benchmark| {
            benchmark.spawn_instances(instances);
            benchmark.run_schedule();
            divan::black_box(benchmark)
        });
}

#[divan::bench(args = [1, 1_000, 10_000], sample_count = 5)]
fn process_toggle(bencher: Bencher<'_, '_>, instances: usize) {
    profile_processing(bencher, instances, support::toggle_definition);
}

#[divan::bench(args = [1, 1_000, 10_000], sample_count = 5)]
fn process_luau(bencher: Bencher<'_, '_>, instances: usize) {
    profile_processing(bencher, instances, support::luau_definition);
}

#[divan::bench(args = [1, 1_000, 10_000], sample_count = 5)]
fn process_typed_effect(bencher: Bencher<'_, '_>, instances: usize) {
    profile_effect_processing(bencher, instances);
}

#[divan::bench(args = [2, 8, 32], sample_count = 5)]
fn process_parallel_width(bencher: Bencher<'_, '_>, regions: usize) {
    profile_processing(bencher, 1_000, || {
        support::wide_parallel_definition(regions)
    });
}

#[divan::bench(args = [4, 16, 64], sample_count = 5)]
fn process_depth(bencher: Bencher<'_, '_>, depth: usize) {
    profile_processing(bencher, 1_000, || support::deep_definition(depth));
}

#[divan::bench(args = [1, 1_000, 10_000], sample_count = 5)]
fn initialize_toggle(bencher: Bencher<'_, '_>, instances: usize) {
    profile_initialization(bencher, instances, support::toggle_definition);
}

#[divan::bench(args = [1, 1_000, 10_000], sample_count = 5)]
fn initialize_luau(bencher: Bencher<'_, '_>, instances: usize) {
    profile_initialization(bencher, instances, support::luau_definition);
}

#[divan::bench(args = [1, 1_000, 10_000], sample_count = 5)]
fn initialize_parallel(bencher: Bencher<'_, '_>, instances: usize) {
    profile_initialization(bencher, instances, support::parallel_definition);
}

#[divan::bench(args = [1, 1_000, 10_000], sample_count = 5)]
fn spawn_and_initialize_toggle(bencher: Bencher<'_, '_>, instances: usize) {
    profile_spawn_and_initialization(bencher, instances, support::toggle_definition);
}

#[divan::bench(args = [1, 1_000, 10_000], sample_count = 5)]
fn spawn_and_initialize_luau(bencher: Bencher<'_, '_>, instances: usize) {
    profile_spawn_and_initialization(bencher, instances, support::luau_definition);
}

#[divan::bench(args = [1, 1_000, 10_000], sample_count = 5)]
fn spawn_and_initialize_parallel(bencher: Bencher<'_, '_>, instances: usize) {
    profile_spawn_and_initialization(bencher, instances, support::parallel_definition);
}
