use std::{collections::BTreeSet, fs, num::NonZeroUsize, path::Path};

use bevy_state_tree::{
    InstanceStatus, StateTreeEventLog, StateTreeEvents, StateTreeExecution, scxml,
};

const CASES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/scxml");
const MICROSTEP_LIMIT: NonZeroUsize = NonZeroUsize::new(1024).unwrap();

#[derive(Clone, Copy)]
struct HarnessEvent(usize);

impl StateTreeEvents for HarnessEvent {
    const AUTHORED_NAMES: &'static [&'static str] =
        &["a", "a.b", "advance", "finish", "reset", "tick"];

    fn event_index(&self) -> usize {
        self.0
    }
}

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

        let mut log = StateTreeEventLog::new();
        let mut execution =
            StateTreeExecution::<HarnessEvent>::new_traced(&definition, MICROSTEP_LIMIT, &mut log);
        let events_path = case.with_extension("events");
        if events_path.exists() {
            let events = fs::read_to_string(&events_path).expect("read SCXML case events");
            for (line_index, line) in events.lines().enumerate() {
                let name = line.trim();
                if name.is_empty() || name.starts_with('#') {
                    continue;
                }
                let Some(index) = HarnessEvent::AUTHORED_NAMES
                    .iter()
                    .position(|candidate| *candidate == name)
                else {
                    failures.push(format!(
                        "{}:{}: undeclared harness event {name:?}",
                        events_path.display(),
                        line_index + 1
                    ));
                    continue;
                };
                if execution.status() != &InstanceStatus::Running {
                    failures.push(format!(
                        "{}:{}: event {name:?} follows terminal status {:?}",
                        events_path.display(),
                        line_index + 1,
                        execution.status()
                    ));
                    break;
                }
                execution.process_event_traced(&HarnessEvent(index), MICROSTEP_LIMIT, &mut log);
            }
        }

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
