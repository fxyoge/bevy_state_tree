use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    num::NonZeroUsize,
    path::Path,
};

use bevy_state_tree::{InstanceStatus, StateTreeEvents, StateTreeExecution, scxml};
use quick_xml::{Reader, XmlVersion, events::Event};
use sha2::{Digest, Sha256};

const SUITE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/w3c");
const VENDOR_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/w3c/vendor");
const CONFORMANCE_NAMESPACE: &str = "http://www.w3.org/2005/scxml-conformance";
const MICROSTEP_LIMIT: NonZeroUsize = NonZeroUsize::new(1024).unwrap();

#[derive(Debug)]
struct ManifestTest {
    mandatory: bool,
    manual: bool,
    starts: Vec<String>,
    dependencies: Vec<String>,
}

#[derive(Debug)]
struct ExecutionResult {
    passed: bool,
    category: &'static str,
    detail: String,
}

#[derive(Debug, Eq, PartialEq)]
enum ExpectedOutcome {
    Pass,
    Fail(String),
}

impl ExpectedOutcome {
    fn passed(&self) -> bool {
        matches!(self, Self::Pass)
    }
}

enum NoEvents {}

impl StateTreeEvents for NoEvents {
    const AUTHORED_NAMES: &'static [&'static str] = &[];

    fn event_index(&self) -> usize {
        match *self {}
    }
}

#[test]
fn vendored_corpus_matches_its_reviewed_lock() {
    let expected = parse_lock(&Path::new(SUITE_ROOT).join("vendor.lock.toml"));
    let actual = files_below(Path::new(VENDOR_ROOT));

    assert_eq!(actual, expected.keys().cloned().collect());
    for (path, (expected_digest, expected_bytes)) in expected {
        let bytes = fs::read(Path::new(VENDOR_ROOT).join(&path)).unwrap();
        assert_eq!(bytes.len() as u64, expected_bytes, "size of {path}");
        assert_eq!(
            format!("{:x}", Sha256::digest(&bytes)),
            expected_digest,
            "digest of {path}"
        );
    }
}

#[test]
fn manifest_accounts_for_the_complete_pinned_suite() {
    let tests = parse_manifest();
    assert_eq!(tests.len(), 200);
    assert_eq!(
        tests.iter().map(|test| test.starts.len()).sum::<usize>(),
        202
    );
    assert_eq!(
        tests
            .iter()
            .map(|test| test.dependencies.len())
            .sum::<usize>(),
        9
    );
    assert_eq!(tests.iter().filter(|test| test.mandatory).count(), 166);
    assert_eq!(tests.iter().filter(|test| !test.mandatory).count(), 34);
    assert_eq!(tests.iter().filter(|test| test.manual).count(), 9);

    for resource in tests
        .iter()
        .flat_map(|test| test.starts.iter().chain(&test.dependencies))
    {
        assert!(
            Path::new(VENDOR_ROOT).join(resource).is_file(),
            "manifest resource {resource} is missing"
        );
    }
}

#[test]
fn parser_accepts_every_xml_resource_without_interpreting_conf_extensions() {
    let tests = parse_manifest();
    let resources: BTreeSet<_> = tests
        .iter()
        .flat_map(|test| test.starts.iter().chain(&test.dependencies))
        .filter(|path| path.ends_with(".txml"))
        .cloned()
        .collect();
    assert_eq!(resources.len(), 206);

    let mut failures = Vec::new();
    for resource in resources {
        let source = fs::read_to_string(Path::new(VENDOR_ROOT).join(&resource)).unwrap();
        if let Err(error) = scxml::parse(&resource, &source) {
            failures.push(error.to_string());
        }
    }
    assert!(
        failures.is_empty(),
        "{} W3C resources failed to parse:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn every_conf_construct_is_explicitly_known_to_the_test_profile() {
    let known_elements: BTreeSet<_> = [
        "array123",
        "concatVars",
        "contentFoo",
        "extendArray",
        "fail",
        "incrementID",
        "pass",
        "script",
        "sendToSender",
        "someInlineVal",
        "sumVars",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    let known_attributes: BTreeSet<_> = [
        "VarEqVar",
        "VarEqVarStruct",
        "arrayTextVar",
        "arrayVar",
        "basicHTTPAccessURITarget",
        "compareIDVal",
        "datamodel",
        "delay",
        "delayFromVar",
        "emptyEventData",
        "eventDataFieldValue",
        "eventDataNamelistValue",
        "eventDataParamValue",
        "eventExpr",
        "eventField",
        "eventFieldHasNoValue",
        "eventFieldsAreBound",
        "eventIdParamHasValue",
        "eventName",
        "eventNameVal",
        "eventNamedParamHasValue",
        "eventRaw",
        "eventSendid",
        "eventType",
        "eventdataSomeVal",
        "eventdataVal",
        "eventvarVal",
        "expr",
        "false",
        "id",
        "idQuoteVal",
        "idSystemVarVal",
        "idVal",
        "idlocation",
        "illegalArray",
        "illegalExpr",
        "illegalItem",
        "illegalTarget",
        "inState",
        "index",
        "invalidLocation",
        "invalidNamelist",
        "invalidSendType",
        "invalidSendTypeExpr",
        "invalidSessionID",
        "isBound",
        "item",
        "location",
        "messageBodyEquals",
        "methodIsPost",
        "name",
        "nameVarVal",
        "namelist",
        "namelistIdVal",
        "noValue",
        "nonBoolean",
        "originTypeEq",
        "quoteExpr",
        "scxmlEventIOLocation",
        "scriptBadSrc",
        "sendIDExpr",
        "srcExpr",
        "systemVarExpr",
        "systemVarIsBound",
        "systemVarLocation",
        "targetExpr",
        "targetVar",
        "targetfail",
        "targetpass",
        "true",
        "typeExpr",
        "unboundVar",
        "unreachableTarget",
        "varChildExpr",
        "varExpr",
        "varNonexistentStruct",
        "varPrefix",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();

    let (elements, attributes) = conformance_names();
    assert_eq!(elements, known_elements);
    assert_eq!(attributes, known_attributes);
}

#[test]
fn manifest_starts_match_expected_w3c_outcomes() {
    let tests = parse_manifest();
    let expected = parse_expected();
    let manifest_starts: BTreeSet<_> = tests.iter().flat_map(|test| &test.starts).collect();
    let expected_starts: BTreeSet<_> = expected.keys().collect();
    let mut mismatches = Vec::new();

    for resource in manifest_starts.difference(&expected_starts) {
        mismatches.push(format!("missing expected result for {resource}"));
    }
    for resource in expected_starts.difference(&manifest_starts) {
        mismatches.push(format!("stale expected result for {resource}"));
    }

    let mut observed_passes = 0;
    let mut categories = BTreeMap::new();
    for test in &tests {
        for resource in &test.starts {
            let result = execute_start(resource, test.manual);
            observed_passes += usize::from(result.passed);
            *categories.entry(result.category).or_insert(0usize) += 1;
            if let Some(expected) = expected.get(resource)
                && expected.passed() != result.passed
            {
                let expectation = match expected {
                    ExpectedOutcome::Pass => "expected pass".to_owned(),
                    ExpectedOutcome::Fail(reason) => {
                        format!("expected failure because {reason}")
                    }
                };
                mismatches.push(format!(
                    "{resource}: {expectation}, observed {} ({}){}",
                    result.passed,
                    result.category,
                    if result.detail.is_empty() {
                        String::new()
                    } else {
                        format!(": {}", result.detail)
                    }
                ));
            }
        }
    }

    println!(
        "W3C observed pass count: {observed_passes}/{}; categories: {categories:?}",
        manifest_starts.len()
    );
    assert!(
        mismatches.is_empty(),
        "W3C expected-result mismatches ({}):\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}

fn parse_expected() -> BTreeMap<String, ExpectedOutcome> {
    let source = fs::read_to_string(Path::new(SUITE_ROOT).join("expected.toml")).unwrap();
    parse_expected_source(&source).unwrap_or_else(|error| panic!("invalid expected.toml: {error}"))
}

fn parse_expected_source(source: &str) -> Result<BTreeMap<String, ExpectedOutcome>, String> {
    let mut document = source
        .parse::<toml::Table>()
        .map_err(|error| error.to_string())?;
    let results = document
        .remove("results")
        .and_then(|value| match value {
            toml::Value::Table(table) => Some(table),
            _ => None,
        })
        .ok_or_else(|| "expected a [results] table".to_owned())?;
    results
        .into_iter()
        .map(|(resource, value)| {
            let outcome = match value {
                toml::Value::Boolean(true) => ExpectedOutcome::Pass,
                toml::Value::String(reason) if !reason.trim().is_empty() => {
                    ExpectedOutcome::Fail(reason)
                }
                toml::Value::Boolean(false) => {
                    return Err(format!(
                        "{resource}: use a failure reason string instead of false"
                    ));
                }
                toml::Value::String(_) => {
                    return Err(format!("{resource}: failure reason must not be empty"));
                }
                _ => {
                    return Err(format!(
                        "{resource}: expected true or a failure reason string"
                    ));
                }
            };
            Ok((resource, outcome))
        })
        .collect()
}

#[test]
fn expected_outcomes_require_reasons_for_failures() {
    let expected = parse_expected_source(
        r#"
            [results]
            "pass.txml" = true
            "fail.txml" = "requires parallel states"
        "#,
    )
    .unwrap();

    assert_eq!(expected["pass.txml"], ExpectedOutcome::Pass);
    assert_eq!(
        expected["fail.txml"],
        ExpectedOutcome::Fail("requires parallel states".to_owned())
    );
    assert!(parse_expected_source("[results]\n\"test.txml\" = false").is_err());
    assert!(parse_expected_source("[results]\n\"test.txml\" = \"  \"").is_err());
    assert!(parse_expected_source("[results]\n\"test.txml\" = 1").is_err());
}

fn execute_start(resource: &str, manual: bool) -> ExecutionResult {
    if manual {
        return ExecutionResult {
            passed: false,
            category: "manual",
            detail: "manual start skipped".to_owned(),
        };
    }
    if !resource.ends_with(".txml") {
        return ExecutionResult {
            passed: false,
            category: "non_xml",
            detail: "non-XML start skipped".to_owned(),
        };
    }

    let source_path = Path::new(VENDOR_ROOT).join(resource);
    let source = match fs::read_to_string(&source_path) {
        Ok(source) => source,
        Err(error) => {
            return ExecutionResult {
                passed: false,
                category: "read_error",
                detail: error.to_string(),
            };
        }
    };
    let mut document = match scxml::parse(resource, &source) {
        Ok(document) => document,
        Err(error) => {
            return ExecutionResult {
                passed: false,
                category: "parse_error",
                detail: error.to_string(),
            };
        }
    };
    normalize_test_profile(&mut document);
    let definition = match scxml::compile(&document) {
        Ok(definition) => definition,
        Err(error) => {
            let category = match error {
                scxml::CompileError::Unsupported { .. } => "unsupported",
                _ => "compile_error",
            };
            return ExecutionResult {
                passed: false,
                category,
                detail: error.to_string(),
            };
        }
    };

    let execution = StateTreeExecution::<NoEvents>::new(&definition, MICROSTEP_LIMIT);
    let active: Vec<_> = execution.active_state_names().map(str::to_owned).collect();
    match execution.status() {
        InstanceStatus::Complete if active.last().is_some_and(|name| name == "pass") => {
            ExecutionResult {
                passed: true,
                category: "pass",
                detail: active.join("/"),
            }
        }
        InstanceStatus::Complete if active.last().is_some_and(|name| name == "fail") => {
            ExecutionResult {
                passed: false,
                category: "explicit_fail",
                detail: active.join("/"),
            }
        }
        InstanceStatus::Complete => ExecutionResult {
            passed: false,
            category: "unexpected_terminal",
            detail: active.join("/"),
        },
        InstanceStatus::Faulted(fault) => ExecutionResult {
            passed: false,
            category: "fault",
            detail: format!("{fault:?}; active={}", active.join("/")),
        },
        InstanceStatus::Pending | InstanceStatus::Running => ExecutionResult {
            passed: false,
            category: "no_outcome",
            detail: format!(
                "status={:?}; active={}",
                execution.status(),
                active.join("/")
            ),
        },
    }
}

fn normalize_test_profile(document: &mut scxml::Document) {
    document
        .root
        .attributes
        .retain(|attribute| !is_conformance_attribute(attribute, "datamodel"));
    for child in &mut document.root.children {
        match child {
            scxml::RootChild::Extension(extension)
                if extension.namespace.as_deref() == Some(CONFORMANCE_NAMESPACE)
                    && matches!(extension.local_name.as_str(), "pass" | "fail") =>
            {
                let id = extension.local_name.clone();
                *child = scxml::RootChild::Final(scxml::Final {
                    span: extension.span,
                    id: Some(id),
                    attributes: Vec::new(),
                    children: Vec::new(),
                });
            }
            _ => {}
        }
    }
    for child in &mut document.root.children {
        normalize_root_child(child);
    }
}

fn normalize_root_child(child: &mut scxml::RootChild) {
    match child {
        scxml::RootChild::State(state) => normalize_state(state),
        scxml::RootChild::Parallel(parallel) => {
            for child in &mut parallel.children {
                normalize_parallel_child(child);
            }
        }
        _ => {}
    }
}

fn normalize_state(state: &mut scxml::State) {
    for child in &mut state.children {
        match child {
            scxml::StateChild::State(state) => normalize_state(state),
            scxml::StateChild::Parallel(parallel) => {
                for child in &mut parallel.children {
                    normalize_parallel_child(child);
                }
            }
            scxml::StateChild::Initial(initial) => {
                if let Some(transition) = &mut initial.transition {
                    normalize_transition(transition);
                }
            }
            scxml::StateChild::History(history) => {
                for child in &mut history.children {
                    if let scxml::HistoryChild::Transition(transition) = child {
                        normalize_transition(transition);
                    }
                }
            }
            scxml::StateChild::Transition(transition) => normalize_transition(transition),
            _ => {}
        }
    }
}

fn normalize_parallel_child(child: &mut scxml::ParallelChild) {
    match child {
        scxml::ParallelChild::State(state) => normalize_state(state),
        scxml::ParallelChild::Parallel(parallel) => {
            for child in &mut parallel.children {
                normalize_parallel_child(child);
            }
        }
        scxml::ParallelChild::History(history) => {
            for child in &mut history.children {
                if let scxml::HistoryChild::Transition(transition) = child {
                    normalize_transition(transition);
                }
            }
        }
        scxml::ParallelChild::Transition(transition) => normalize_transition(transition),
        _ => {}
    }
}

fn normalize_transition(transition: &mut scxml::Transition) {
    let target = transition.attributes.iter().find_map(|attribute| {
        is_conformance_attribute(attribute, "targetpass")
            .then_some("pass")
            .or_else(|| is_conformance_attribute(attribute, "targetfail").then_some("fail"))
    });
    if let Some(target) = target {
        transition.targets = vec![target.to_owned()];
        transition.attributes.retain(|attribute| {
            !is_conformance_attribute(attribute, "targetpass")
                && !is_conformance_attribute(attribute, "targetfail")
        });
    }
}

fn is_conformance_attribute(attribute: &scxml::Attribute, local_name: &str) -> bool {
    attribute.namespace.as_deref() == Some(CONFORMANCE_NAMESPACE)
        && attribute.local_name == local_name
}

fn parse_manifest() -> Vec<ManifestTest> {
    let source = fs::read_to_string(Path::new(VENDOR_ROOT).join("manifest.xml")).unwrap();
    let mut reader = Reader::from_str(&source);
    let mut tests = Vec::new();
    let mut current = None;
    loop {
        match reader.read_event().unwrap() {
            Event::Start(element) if element.local_name().as_ref() == b"test" => {
                let attributes = attributes(&element);
                current = Some(ManifestTest {
                    mandatory: attributes.get("conformance").unwrap() == "mandatory",
                    manual: attributes.get("manual").unwrap() == "true",
                    starts: Vec::new(),
                    dependencies: Vec::new(),
                });
            }
            Event::Empty(element) if element.local_name().as_ref() == b"start" => {
                current
                    .as_mut()
                    .unwrap()
                    .starts
                    .push(attributes(&element).remove("uri").unwrap());
            }
            Event::Empty(element) if element.local_name().as_ref() == b"dep" => {
                current
                    .as_mut()
                    .unwrap()
                    .dependencies
                    .push(attributes(&element).remove("uri").unwrap());
            }
            Event::End(element) if element.local_name().as_ref() == b"test" => {
                tests.push(current.take().unwrap());
            }
            Event::Eof => return tests,
            _ => {}
        }
    }
}

fn conformance_names() -> (BTreeSet<String>, BTreeSet<String>) {
    let mut elements = BTreeSet::new();
    let mut attributes = BTreeSet::new();
    for path in files_below(Path::new(VENDOR_ROOT)) {
        if !path.ends_with(".txml") {
            continue;
        }
        let source = fs::read_to_string(Path::new(VENDOR_ROOT).join(path)).unwrap();
        let mut reader = Reader::from_str(&source);
        let mut conf_prefixes = BTreeSet::new();
        loop {
            match reader.read_event().unwrap() {
                Event::Start(element) | Event::Empty(element) => {
                    for attribute in element.attributes().map(Result::unwrap) {
                        let name = String::from_utf8_lossy(attribute.key.as_ref());
                        if name == "xmlns:conf"
                            && attribute.normalized_value(XmlVersion::Implicit1_0).unwrap()
                                == CONFORMANCE_NAMESPACE
                        {
                            conf_prefixes.insert("conf".to_owned());
                        }
                    }
                    let name = String::from_utf8_lossy(element.name().as_ref()).into_owned();
                    if let Some((prefix, local)) = name.split_once(':')
                        && conf_prefixes.contains(prefix)
                    {
                        elements.insert(local.to_owned());
                    }
                    for attribute in element.attributes().map(Result::unwrap) {
                        let name = String::from_utf8_lossy(attribute.key.as_ref());
                        if let Some((prefix, local)) = name.split_once(':')
                            && conf_prefixes.contains(prefix)
                        {
                            attributes.insert(local.to_owned());
                        }
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }
    }
    (elements, attributes)
}

fn attributes(element: &quick_xml::events::BytesStart<'_>) -> BTreeMap<String, String> {
    element
        .attributes()
        .map(|attribute| {
            let attribute = attribute.unwrap();
            (
                String::from_utf8_lossy(attribute.key.as_ref()).into_owned(),
                attribute
                    .normalized_value(XmlVersion::Implicit1_0)
                    .unwrap()
                    .into_owned(),
            )
        })
        .collect()
}

fn parse_lock(path: &Path) -> BTreeMap<String, (String, u64)> {
    let source = fs::read_to_string(path).unwrap();
    let mut files = BTreeMap::new();
    let mut path = None;
    let mut digest = None;
    for line in source.lines() {
        if let Some(value) = line.strip_prefix("path = \"") {
            path = Some(value.strip_suffix('"').unwrap().to_owned());
        } else if let Some(value) = line.strip_prefix("sha256 = \"") {
            digest = Some(value.strip_suffix('"').unwrap().to_owned());
        } else if let Some(value) = line.strip_prefix("bytes = ") {
            files.insert(
                path.take().unwrap(),
                (digest.take().unwrap(), value.parse().unwrap()),
            );
        }
    }
    files
}

fn files_below(root: &Path) -> BTreeSet<String> {
    fn visit(root: &Path, directory: &Path, files: &mut BTreeSet<String>) {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(root, &path, files);
            } else {
                files.insert(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    let mut files = BTreeSet::new();
    visit(root, root, &mut files);
    files
}
