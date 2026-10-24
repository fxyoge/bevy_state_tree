use std::{collections::BTreeSet, fs, num::NonZeroUsize, path::Path};

use bevy_app::{App, Update};
use bevy_asset::{AssetPlugin, Assets};
use bevy_ecs::{
    event::Event,
    system::{In, RunSystemOnce},
};
use bevy_state_tree::{
    BroadcastStateTreeEvent, BroadcastStateTreeEventWriter, InstanceStatus, StateTreeConfig,
    StateTreeDefinition, StateTreeEvent, StateTreeInstance, StateTreePlugin, StateTreeSettings,
    StateTreeTraceLog, StateTreeTracePlugin, scxml,
};
use bevy_time::TimePlugin;

const CASES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/scxml");
const MICROSTEP_LIMIT: NonZeroUsize = NonZeroUsize::new(1024).unwrap();

fn configure_harness(config: &mut StateTreeConfig<'_>) {
    config
        .event::<A>()
        .event::<AB>()
        .event::<Advance>()
        .event::<Finish>()
        .event::<Reset>()
        .event::<Tick>();
}

#[derive(Event, StateTreeEvent)]
#[state_tree(name = "a", broadcast)]
struct A;

#[derive(Event, StateTreeEvent)]
#[state_tree(name = "a.b", broadcast)]
struct AB;

#[derive(Event, StateTreeEvent)]
#[state_tree(name = "advance", broadcast)]
struct Advance;

#[derive(Event, StateTreeEvent)]
#[state_tree(name = "finish", broadcast)]
struct Finish;

#[derive(Event, StateTreeEvent)]
#[state_tree(name = "reset", broadcast)]
struct Reset;

#[derive(Event, StateTreeEvent)]
#[state_tree(name = "tick", broadcast)]
struct Tick;

#[test]
fn scxml_cases_match_their_semantic_traces() {
    let root = Path::new(CASES);
    let manifest_path = root.join("manifest.txt");
    let expected_cases: BTreeSet<_> = fs::read_to_string(&manifest_path)
        .expect("read SCXML case manifest")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|name| root.join(name))
        .collect();
    let mut sources = BTreeSet::new();
    let mut traces = BTreeSet::new();
    let mut event_streams = BTreeSet::new();
    for entry in fs::read_dir(root).expect("read SCXML trace cases") {
        let path = entry.expect("read SCXML trace case entry").path();
        if path == manifest_path {
            continue;
        }
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("scxml") => {
                sources.insert(path.with_extension(""));
            }
            Some("trace") => {
                traces.insert(path.with_extension(""));
            }
            Some("events") => {
                event_streams.insert(path.with_extension(""));
            }
            _ => panic!("unexpected SCXML trace case file: {}", path.display()),
        }
    }

    let mut failures = Vec::new();
    for missing in expected_cases.difference(&sources) {
        failures.push(format!("manifest case missing {}.scxml", missing.display()));
    }
    for unlisted in sources.difference(&expected_cases) {
        failures.push(format!("unlisted SCXML case {}.scxml", unlisted.display()));
    }
    for missing in sources.difference(&traces) {
        failures.push(format!("missing {}.trace", missing.display()));
    }
    for stale in traces.difference(&sources) {
        failures.push(format!("stale {}.trace", stale.display()));
    }
    for stale in event_streams.difference(&sources) {
        failures.push(format!("stale {}.events", stale.display()));
    }

    for case in sources.intersection(&traces) {
        let source_path = case.with_extension("scxml");
        let trace_path = case.with_extension("trace");
        let source = fs::read_to_string(&source_path).expect("read SCXML case");
        let source_name = source_path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("UTF-8 SCXML case name");
        let document = match scxml::parse(source_name, &source) {
            Ok(document) => document,
            Err(error) => {
                failures.push(format!("{source_name}: {error}"));
                continue;
            }
        };
        let definition = match scxml::compile(&document) {
            Ok(definition) => definition,
            Err(error) => {
                failures.push(format!("{source_name}: {error}"));
                continue;
            }
        };

        let mut app = App::new();
        app.add_plugins((
            TimePlugin,
            AssetPlugin::default(),
            StateTreeTracePlugin,
            StateTreePlugin::new(Update).configure(configure_harness),
        ));
        app.insert_resource(StateTreeSettings::new(MICROSTEP_LIMIT));
        let handle = app
            .world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .add(definition);
        let entity = app.world_mut().spawn(StateTreeInstance::new(handle)).id();
        app.update();
        let events_path = case.with_extension("events");
        if events_path.exists() {
            let events = fs::read_to_string(&events_path).expect("read SCXML case events");
            for (line_index, line) in events.lines().enumerate() {
                let name = line.trim();
                if name.is_empty() || name.starts_with('#') {
                    continue;
                }
                let status = app
                    .world()
                    .get::<StateTreeInstance>(entity)
                    .unwrap()
                    .status();
                if status != &InstanceStatus::Running {
                    failures.push(format!(
                        "{}:{}: event {name:?} follows terminal status {:?}",
                        events_path.display(),
                        line_index + 1,
                        status
                    ));
                    break;
                }
                match name {
                    "a" => queue(&mut app, A),
                    "a.b" => queue(&mut app, AB),
                    "advance" => queue(&mut app, Advance),
                    "finish" => queue(&mut app, Finish),
                    "reset" => queue(&mut app, Reset),
                    "tick" => queue(&mut app, Tick),
                    _ => {
                        failures.push(format!(
                            "{}:{}: undeclared harness event {name:?}",
                            events_path.display(),
                            line_index + 1
                        ));
                        continue;
                    }
                }
            }
        }

        let log = app
            .world()
            .resource::<StateTreeTraceLog>()
            .get(entity)
            .unwrap();
        let expected = fs::read_to_string(&trace_path).expect("read expected SCXML trace");
        if expected != log.as_str() {
            failures.push(format!(
                "{source_name}: trace mismatch\nexpected:\n{expected}\nactual:\n{}",
                log.as_str()
            ));
        }
    }

    if !failures.is_empty() {
        panic!(
            "{} SCXML trace case failure(s):\n{}",
            failures.len(),
            failures.join("\n\n")
        );
    }
}

fn queue<E: BroadcastStateTreeEvent>(app: &mut App, event: E) {
    app.world_mut()
        .run_system_once_with(queue_event::<E>, event)
        .unwrap();
    app.update();
}

fn queue_event<E: BroadcastStateTreeEvent>(
    In(event): In<E>,
    mut writer: BroadcastStateTreeEventWriter,
) {
    writer.write(event);
}
