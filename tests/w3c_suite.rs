use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use bevy_state_tree::scxml;
use quick_xml::{Reader, XmlVersion, events::Event};
use sha2::{Digest, Sha256};

const SUITE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/w3c");
const VENDOR_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/w3c/vendor");
const CONFORMANCE_NAMESPACE: &str = "http://www.w3.org/2005/scxml-conformance";

#[derive(Debug)]
struct ManifestTest {
    mandatory: bool,
    manual: bool,
    starts: Vec<String>,
    dependencies: Vec<String>,
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
