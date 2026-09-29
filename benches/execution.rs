use std::num::NonZeroUsize;

use bevy_state_tree::{
    StateTreeBuilder, StateTreeEventLog, StateTreeEvents, StateTreeExecution, scxml,
};
use divan::{Bencher, black_box};

const LIMIT: NonZeroUsize = NonZeroUsize::new(1024).unwrap();

fn main() {
    divan::main();
}

enum NoEvents {}

impl StateTreeEvents for NoEvents {
    const AUTHORED_NAMES: &'static [&'static str] = &[];

    fn event_index(&self) -> usize {
        match *self {}
    }
}

enum Events {
    Tick,
}

impl StateTreeEvents for Events {
    const AUTHORED_NAMES: &'static [&'static str] = &["tick"];

    fn event_index(&self) -> usize {
        match self {
            Self::Tick => 0,
        }
    }
}

#[divan::bench(args = [1, 8, 32, 128])]
fn initialize_hierarchy(bencher: Bencher<'_, '_>, depth: usize) {
    let mut builder = StateTreeBuilder::new("hierarchy");
    let root = builder.compound_root("root").unwrap();
    let mut parent = root;
    for index in 0..depth {
        let child = builder.compound(format!("state-{index}"), parent).unwrap();
        builder.initial(parent, child).unwrap();
        parent = child;
    }
    let leaf = builder.atomic("leaf", parent).unwrap();
    builder.initial(parent, leaf).unwrap();
    let definition = builder.build().unwrap();

    bencher.bench_local(|| black_box(StateTreeExecution::<NoEvents>::new(&definition, LIMIT)));
}

#[divan::bench(args = [1, 8, 32, 128])]
fn initialize_and_stabilize_eventless_chain(bencher: Bencher<'_, '_>, transitions: usize) {
    let mut builder = StateTreeBuilder::new("eventless-chain");
    let root = builder.compound_root("root").unwrap();
    let mut source = builder.atomic("state-0", root).unwrap();
    builder.initial(root, source).unwrap();
    for index in 1..=transitions {
        let target = builder.atomic(format!("state-{index}"), root).unwrap();
        builder.eventless(source, target).unwrap();
        source = target;
    }
    let definition = builder.build().unwrap();

    bencher.bench_local(|| black_box(StateTreeExecution::<NoEvents>::new(&definition, LIMIT)));
}

#[divan::bench]
fn process_external_transition(bencher: Bencher<'_, '_>) {
    let mut builder = StateTreeBuilder::new("external");
    let root = builder.compound_root("root").unwrap();
    let idle = builder.atomic("idle", root).unwrap();
    let done = builder.final_state("done", root).unwrap();
    builder.initial(root, idle).unwrap();
    builder.transition(idle, "tick", done).unwrap();
    let definition = builder.build().unwrap();

    bencher
        .with_inputs(|| StateTreeExecution::<Events>::new(&definition, LIMIT))
        .bench_local_values(|mut execution| {
            let handled = black_box(execution.process_event(&Events::Tick, LIMIT));
            (execution, handled)
        });
}

#[divan::bench]
fn process_ignored_external_event(bencher: Bencher<'_, '_>) {
    let mut builder = StateTreeBuilder::new("ignored-external");
    let root = builder.compound_root("root").unwrap();
    let idle = builder.atomic("idle", root).unwrap();
    let done = builder.final_state("done", root).unwrap();
    builder.initial(root, idle).unwrap();
    builder.transition(idle, "other", done).unwrap();
    let definition = builder.build().unwrap();

    bencher
        .with_inputs(|| StateTreeExecution::<Events>::new(&definition, LIMIT))
        .bench_local_values(|mut execution| {
            let handled = black_box(execution.process_event(&Events::Tick, LIMIT));
            (execution, handled)
        });
}

#[divan::bench(args = [1, 8, 32, 128])]
fn select_transition_fanout(bencher: Bencher<'_, '_>, preceding: usize) {
    let mut builder = StateTreeBuilder::new("transition-fanout");
    let root = builder.compound_root("root").unwrap();
    let idle = builder.atomic("idle", root).unwrap();
    let done = builder.final_state("done", root).unwrap();
    builder.initial(root, idle).unwrap();
    for index in 0..preceding {
        builder
            .transition(idle, format!("other-{index}"), done)
            .unwrap();
    }
    builder.transition(idle, "tick", done).unwrap();
    let definition = builder.build().unwrap();

    bencher
        .with_inputs(|| StateTreeExecution::<Events>::new(&definition, LIMIT))
        .bench_local_values(|mut execution| {
            let handled = black_box(execution.process_event(&Events::Tick, LIMIT));
            (execution, handled)
        });
}

#[divan::bench]
fn process_luau_guard_and_assignment(bencher: Bencher<'_, '_>) {
    let source = r#"
        <scxml xmlns="http://www.w3.org/2005/07/scxml" datamodel="luau">
          <datamodel><data id="count" expr="0"/></datamodel>
          <state id="idle">
            <transition event="tick" cond="count == 0" target="done">
              <assign location="count" expr="count + 1"/>
            </transition>
          </state>
          <final id="done"/>
        </scxml>
    "#;
    let document = scxml::parse("benchmark.scxml", source).unwrap();
    let definition = scxml::compile(&document).unwrap();

    bencher
        .with_inputs(|| StateTreeExecution::<Events>::new(&definition, LIMIT))
        .bench_local_values(|mut execution| {
            let handled = black_box(execution.process_event(&Events::Tick, LIMIT));
            (execution, handled)
        });
}

#[divan::bench]
fn process_external_transition_with_trace(bencher: Bencher<'_, '_>) {
    let mut builder = StateTreeBuilder::new("traced-external");
    let root = builder.compound_root("root").unwrap();
    let idle = builder.atomic("idle", root).unwrap();
    let done = builder.final_state("done", root).unwrap();
    builder.initial(root, idle).unwrap();
    builder.transition(idle, "tick", done).unwrap();
    let definition = builder.build().unwrap();

    bencher
        .with_inputs(|| {
            let mut log = StateTreeEventLog::new();
            let execution = StateTreeExecution::<Events>::new_traced(&definition, LIMIT, &mut log);
            (execution, log)
        })
        .bench_local_values(|(mut execution, mut log)| {
            execution.process_event_traced(&Events::Tick, LIMIT, &mut log);
            black_box((execution, log))
        });
}
