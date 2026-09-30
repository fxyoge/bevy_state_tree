mod support;

use std::{cell::RefCell, time::Duration};

use divan::{Bencher, counter::ItemsCount};
use support::{BenchmarkApp, BenchmarkDefinition, Executor};

fn main() {
    divan::main();
}

fn process_broadcast(
    bencher: Bencher<'_, '_>,
    instances: usize,
    definition: impl Fn() -> BenchmarkDefinition,
) {
    let benchmark = RefCell::new(BenchmarkApp::initialized(
        definition(),
        instances,
        Executor::Parallel,
    ));
    benchmark.borrow_mut().enable_broadcast_events();
    bencher
        .counter(ItemsCount::new(instances))
        .bench_local(|| benchmark.borrow_mut().run_schedule());
}

fn process_effects(bencher: Bencher<'_, '_>, instances: usize) {
    let mut benchmark = BenchmarkApp::empty(support::effect_definition(), Executor::Parallel);
    benchmark.register_benchmark_effect();
    benchmark.spawn_instances(instances);
    benchmark.run_schedule();
    benchmark.enable_broadcast_events();
    let benchmark = RefCell::new(benchmark);
    bencher
        .counter(ItemsCount::new(instances))
        .bench_local(|| benchmark.borrow_mut().run_schedule());
}

fn process_activity_outcomes(bencher: Bencher<'_, '_>, instances: usize) {
    bencher
        .counter(ItemsCount::new(instances))
        .with_inputs(|| {
            let mut benchmark =
                BenchmarkApp::empty(support::activity_definition(), Executor::Parallel);
            benchmark.register_benchmark_activity();
            benchmark.spawn_instances(instances);
            benchmark.run_schedule();
            benchmark.queue_activity_outcomes();
            benchmark
        })
        .bench_local_values(|mut benchmark| {
            benchmark.run_schedule();
            divan::black_box(benchmark)
        });
}

#[divan::bench(args = [1_000, 100_000, 1_000_000], sample_count = 20)]
fn broadcast_toggle(bencher: Bencher<'_, '_>, instances: usize) {
    process_broadcast(bencher, instances, support::toggle_definition);
}

#[divan::bench(args = [1_000, 100_000], sample_count = 10)]
fn broadcast_luau(bencher: Bencher<'_, '_>, instances: usize) {
    process_broadcast(bencher, instances, support::luau_definition);
}

#[divan::bench(args = [1_000, 100_000], sample_count = 10)]
fn broadcast_parallel_two_regions(bencher: Bencher<'_, '_>, instances: usize) {
    process_broadcast(bencher, instances, support::parallel_definition);
}

#[divan::bench(args = [1_000, 100_000], sample_count = 10)]
fn broadcast_shallow_history(bencher: Bencher<'_, '_>, instances: usize) {
    process_broadcast(bencher, instances, support::history_definition);
}

#[divan::bench(args = [1_000, 100_000], sample_count = 10)]
fn broadcast_typed_effect(bencher: Bencher<'_, '_>, instances: usize) {
    process_effects(bencher, instances);
}

#[divan::bench(args = [1_000, 100_000], sample_count = 10)]
fn dense_activity_outcomes(bencher: Bencher<'_, '_>, instances: usize) {
    process_activity_outcomes(bencher, instances);
}

#[divan::bench(args = [4, 16, 64, 128], sample_count = 10)]
fn depth_reentry(bencher: Bencher<'_, '_>, depth: usize) {
    process_broadcast(bencher, 1_000, || support::deep_definition(depth));
}

#[divan::bench(args = [2, 8, 32, 64, 128, 256], sample_count = 10)]
fn parallel_width(bencher: Bencher<'_, '_>, regions: usize) {
    process_broadcast(bencher, 1_000, || {
        support::wide_parallel_definition(regions)
    });
}

#[divan::bench(args = [1, 8, 32, 128], sample_count = 10)]
fn transition_scan_fanout(bencher: Bencher<'_, '_>, candidates: usize) {
    process_broadcast(bencher, 1_000, || {
        support::transition_scan_definition(candidates)
    });
}

#[divan::bench(args = [1, 100, 1_000], sample_count = 10)]
fn sparse_entity_events(bencher: Bencher<'_, '_>, targets: usize) {
    let benchmark = RefCell::new(BenchmarkApp::initialized(
        support::entity_toggle_definition(),
        100_000,
        Executor::Parallel,
    ));
    benchmark.borrow_mut().enable_entity_events(targets);
    bencher
        .counter(ItemsCount::new(targets))
        .bench_local(|| benchmark.borrow_mut().run_schedule());
}

#[divan::bench(args = [1_000, 100_000, 1_000_000], sample_count = 20)]
fn timers_far_future_idle(bencher: Bencher<'_, '_>, instances: usize) {
    let benchmark = RefCell::new(BenchmarkApp::initialized(
        support::timer_definition(Duration::from_secs(3_600)),
        instances,
        Executor::Parallel,
    ));
    bencher.bench_local(|| benchmark.borrow_mut().run_schedule());
}

#[divan::bench(args = [1_000, 100_000], sample_count = 10)]
fn timers_dense_expiry(bencher: Bencher<'_, '_>, instances: usize) {
    bencher
        .counter(ItemsCount::new(instances))
        .with_inputs(|| {
            let mut benchmark = BenchmarkApp::initialized(
                support::timer_definition(Duration::from_secs(1)),
                instances,
                Executor::Parallel,
            );
            benchmark.advance_time(Duration::from_secs(1));
            benchmark
        })
        .bench_local_values(|mut benchmark| {
            benchmark.run_schedule();
            divan::black_box(benchmark)
        });
}

#[divan::bench(args = [1_000, 10_000], sample_count = 5)]
fn initialize_toggle(bencher: Bencher<'_, '_>, instances: usize) {
    bencher
        .counter(ItemsCount::new(instances))
        .with_inputs(|| {
            BenchmarkApp::pending(support::toggle_definition(), instances, Executor::Parallel)
        })
        .bench_local_values(|mut benchmark| {
            benchmark.run_schedule();
            divan::black_box(benchmark)
        });
}

#[divan::bench(args = [1_000, 10_000], sample_count = 5)]
fn initialize_dense_activities(bencher: Bencher<'_, '_>, instances: usize) {
    bencher
        .counter(ItemsCount::new(instances))
        .with_inputs(|| {
            let mut benchmark =
                BenchmarkApp::empty(support::activity_definition(), Executor::Parallel);
            benchmark.register_benchmark_activity();
            benchmark.spawn_instances(instances);
            benchmark
        })
        .bench_local_values(|mut benchmark| {
            benchmark.run_schedule();
            divan::black_box(benchmark)
        });
}
