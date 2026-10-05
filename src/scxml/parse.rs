use std::{
    collections::{BTreeSet, HashMap},
    time::Duration,
};

use quick_xml::events::{BytesRef, BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

use super::ast::*;

/// The namespace URI defined by the SCXML specification.
pub const SCXML_NAMESPACE: &str = "http://www.w3.org/2005/07/scxml";

/// Parse an SCXML source string without compiling it into a runtime model.
pub fn parse(source_name: &str, source: &str) -> Result<Document, Diagnostic> {
    let root = XmlParser::new(source_name, source).parse_document()?;
    validate_known_scxml_names(&root, source_name, source)?;
    let root = decode_root(&root, source_name, source)?;
    Ok(Document {
        source_name: source_name.to_owned(),
        span: root.span,
        root,
    })
}

#[derive(Debug, Clone)]
struct Element {
    span: Span,
    name: String,
    local_name: String,
    namespace: Option<String>,
    attributes: Vec<Attribute>,
    children: Vec<Element>,
    content: Vec<RawContent>,
    text: String,
}

#[derive(Debug, Clone)]
enum RawContent {
    Text(String),
    Element(Element),
}

struct XmlParser<'a> {
    source_name: &'a str,
    source: &'a str,
    reader: Reader<&'a [u8]>,
}

impl<'a> XmlParser<'a> {
    fn new(source_name: &'a str, source: &'a str) -> Self {
        let mut reader = Reader::from_str(source);
        reader.config_mut().trim_text(false);
        Self {
            source_name,
            source,
            reader,
        }
    }

    fn parse_document(mut self) -> Result<Element, Diagnostic> {
        let mut buffer = Vec::new();
        let root = loop {
            let position = self.position();
            match self.reader.read_event_into(&mut buffer) {
                Ok(Event::Start(element)) => {
                    break self.parse_element(position, &element, &HashMap::new(), false)?;
                }
                Ok(Event::Empty(element)) => {
                    break self.empty_element(position, &element, &HashMap::new())?;
                }
                Ok(Event::Text(text)) if text.iter().all(u8::is_ascii_whitespace) => {}
                Ok(Event::CData(text)) if text.iter().all(u8::is_ascii_whitespace) => {}
                Ok(Event::DocType(_)) => {
                    return Err(
                        self.error_at(position, "DOCTYPE and DTD declarations are not supported")
                    );
                }
                Ok(Event::Eof) => {
                    return Err(self.error_at(position, "expected an XML document element"));
                }
                Ok(Event::Text(_)) | Ok(Event::CData(_)) => {
                    return Err(
                        self.error_at(position, "non-whitespace text outside the document element")
                    );
                }
                Ok(Event::GeneralRef(_)) => {
                    return Err(self.error_at(
                        position,
                        "entity references are not allowed outside the document element",
                    ));
                }
                Ok(_) => {}
                Err(error) => return Err(self.error_at(position, error.to_string())),
            }
            buffer.clear();
        };

        loop {
            let position = self.position();
            match self.reader.read_event_into(&mut buffer) {
                Ok(Event::Text(text)) if text.iter().all(u8::is_ascii_whitespace) => {}
                Ok(Event::CData(text)) if text.iter().all(u8::is_ascii_whitespace) => {}
                Ok(Event::DocType(_)) => {
                    return Err(
                        self.error_at(position, "DOCTYPE and DTD declarations are not supported")
                    );
                }
                Ok(Event::Eof) => return Ok(root),
                Ok(Event::Start(_) | Event::Empty(_)) => {
                    return Err(
                        self.error_at(position, "XML documents may contain only one root element")
                    );
                }
                Ok(Event::Text(_)) | Ok(Event::CData(_)) => {
                    return Err(
                        self.error_at(position, "non-whitespace text outside the document element")
                    );
                }
                Ok(Event::GeneralRef(_)) => {
                    return Err(self.error_at(
                        position,
                        "entity references are not allowed outside the document element",
                    ));
                }
                Ok(Event::End(_)) => {
                    return Err(self.error_at(position, "unexpected closing tag"));
                }
                Ok(_) => {}
                Err(error) => return Err(self.error_at(position, error.to_string())),
            }
            buffer.clear();
        }
    }

    fn parse_element(
        &mut self,
        start: usize,
        element: &BytesStart<'_>,
        inherited_namespaces: &HashMap<String, String>,
        allow_text: bool,
    ) -> Result<Element, Diagnostic> {
        let (mut parsed, namespaces) =
            self.element_from_start(start, element, inherited_namespaces)?;
        let text_allowed = allow_text || permits_text(&parsed);
        let expected_name = parsed.name.clone();
        let mut buffer = Vec::new();
        loop {
            let position = self.position();
            match self.reader.read_event_into(&mut buffer) {
                Ok(Event::Start(child)) => {
                    let child = self.parse_element(position, &child, &namespaces, text_allowed)?;
                    parsed.content.push(RawContent::Element(child.clone()));
                    parsed.children.push(child);
                }
                Ok(Event::Empty(child)) => {
                    let child = self.empty_element(position, &child, &namespaces)?;
                    parsed.content.push(RawContent::Element(child.clone()));
                    parsed.children.push(child);
                }
                Ok(Event::Text(text)) => {
                    let text = text
                        .xml_content(XmlVersion::Implicit1_0)
                        .map_err(|error| self.error_at(position, error.to_string()))?
                        .into_owned();
                    if !text_allowed && !is_xml_whitespace(&text) {
                        return Err(self.error_at(
                            position,
                            "non-whitespace direct text is not allowed in structural SCXML elements",
                        ));
                    }
                    parsed.text.push_str(&text);
                    parsed.content.push(RawContent::Text(text));
                }
                Ok(Event::CData(text)) => {
                    let text = text
                        .xml_content(XmlVersion::Implicit1_0)
                        .map_err(|error| self.error_at(position, error.to_string()))?
                        .into_owned();
                    if !text_allowed && !is_xml_whitespace(&text) {
                        return Err(self.error_at(
                            position,
                            "non-whitespace direct text is not allowed in structural SCXML elements",
                        ));
                    }
                    parsed.text.push_str(&text);
                    parsed.content.push(RawContent::Text(text));
                }
                Ok(Event::GeneralRef(reference)) => {
                    let text = self.resolve_general_ref(position, &reference)?;
                    if !text_allowed && !is_xml_whitespace(&text) {
                        return Err(self.error_at(
                            position,
                            "non-whitespace direct text is not allowed in structural SCXML elements",
                        ));
                    }
                    parsed.text.push_str(&text);
                    parsed.content.push(RawContent::Text(text));
                }
                Ok(Event::End(end)) => {
                    let actual_name = String::from_utf8_lossy(end.name().as_ref()).into_owned();
                    if actual_name != expected_name {
                        return Err(self.error_at(position, "mismatched XML closing tag"));
                    }
                    parsed.span.end = self.position();
                    return Ok(parsed);
                }
                Ok(Event::Eof) => {
                    return Err(self.error_at(
                        position,
                        format!("unexpected end of file in <{expected_name}>"),
                    ));
                }
                Ok(Event::DocType(_)) => {
                    return Err(
                        self.error_at(position, "DOCTYPE and DTD declarations are not supported")
                    );
                }
                Ok(_) => {}
                Err(error) => return Err(self.error_at(position, error.to_string())),
            }
            buffer.clear();
        }
    }

    fn empty_element(
        &self,
        start: usize,
        element: &BytesStart<'_>,
        inherited_namespaces: &HashMap<String, String>,
    ) -> Result<Element, Diagnostic> {
        let (mut parsed, _) = self.element_from_start(start, element, inherited_namespaces)?;
        parsed.span.end = self.position();
        Ok(parsed)
    }

    fn element_from_start(
        &self,
        start: usize,
        element: &BytesStart<'_>,
        inherited_namespaces: &HashMap<String, String>,
    ) -> Result<(Element, HashMap<String, String>), Diagnostic> {
        let mut raw_attributes = Vec::new();
        for attribute in element.attributes() {
            let attribute = attribute.map_err(|error| self.error_at(start, error.to_string()))?;
            let value = attribute
                .normalized_value(XmlVersion::Implicit1_0)
                .map_err(|error| self.error_at(start, error.to_string()))?;
            raw_attributes.push((
                String::from_utf8_lossy(attribute.key.as_ref()).into_owned(),
                value.into_owned(),
            ));
        }

        let mut namespaces = inherited_namespaces.clone();
        for (name, value) in &raw_attributes {
            if name == "xmlns" {
                validate_namespace_binding(self, start, None, value)?;
                namespaces.insert(String::new(), value.clone());
            } else if let Some(prefix) = name.strip_prefix("xmlns:") {
                if prefix.is_empty() {
                    return Err(self.error_at(start, "invalid XML namespace declaration"));
                }
                validate_namespace_binding(self, start, Some(prefix), value)?;
                namespaces.insert(prefix.to_owned(), value.clone());
            }
        }

        let mut expanded_names = BTreeSet::new();
        let attributes = raw_attributes
            .into_iter()
            .map(|(name, value)| {
                let (prefix, local_name) = split_name(&name, start, self)?;
                let local_name = local_name.to_owned();
                let namespace = if name == "xmlns" || prefix == Some("xmlns") {
                    Some("http://www.w3.org/2000/xmlns/".to_owned())
                } else if let Some(prefix) = prefix {
                    if prefix == "xml" {
                        Some("http://www.w3.org/XML/1998/namespace".to_owned())
                    } else if prefix == "xmlns" {
                        return Err(self.error_at(start, "the xmlns prefix is reserved"));
                    } else if !namespaces.contains_key(prefix) {
                        return Err(self.error_at(
                            start,
                            format!("undeclared XML namespace prefix `{prefix}`"),
                        ));
                    } else {
                        let namespace = namespaces.get(prefix).cloned().unwrap_or_default();
                        if namespace.is_empty() {
                            return Err(self.error_at(
                                start,
                                format!("prefixed XML namespace `{prefix}` cannot be undeclared"),
                            ));
                        }
                        Some(namespace)
                    }
                } else {
                    None
                };
                if !expanded_names.insert((namespace.clone(), local_name.clone())) {
                    return Err(self.error_at(
                        start,
                        format!("duplicate XML attribute `{name}` after namespace resolution"),
                    ));
                }
                Ok(Attribute {
                    name,
                    local_name,
                    namespace,
                    value,
                })
            })
            .collect::<Result<Vec<_>, Diagnostic>>()?;

        let name = String::from_utf8_lossy(element.name().as_ref()).into_owned();
        let (prefix, local_name) = split_name(&name, start, self)?;
        let local_name = local_name.to_owned();
        let namespace = if let Some(prefix) = prefix {
            if prefix == "xml" {
                Some("http://www.w3.org/XML/1998/namespace".to_owned())
            } else if prefix == "xmlns" {
                return Err(self.error_at(start, "the xmlns prefix is reserved"));
            } else if !namespaces.contains_key(prefix) {
                return Err(
                    self.error_at(start, format!("undeclared XML namespace prefix `{prefix}`"))
                );
            } else {
                let namespace = namespaces.get(prefix).cloned().unwrap_or_default();
                if namespace.is_empty() {
                    return Err(self.error_at(
                        start,
                        format!("prefixed XML namespace `{prefix}` cannot be undeclared"),
                    ));
                }
                Some(namespace)
            }
        } else {
            namespaces.get("").filter(|uri| !uri.is_empty()).cloned()
        };

        Ok((
            Element {
                span: Span { start, end: start },
                name,
                local_name,
                namespace,
                attributes,
                children: Vec::new(),
                content: Vec::new(),
                text: String::new(),
            },
            namespaces,
        ))
    }

    fn error_at(&self, offset: usize, message: impl Into<String>) -> Diagnostic {
        diagnostic_at(self.source_name, self.source, offset, message.into())
    }

    fn position(&self) -> usize {
        self.reader
            .buffer_position()
            .try_into()
            .expect("source position fits in usize")
    }

    fn resolve_general_ref(
        &self,
        offset: usize,
        reference: &BytesRef<'_>,
    ) -> Result<String, Diagnostic> {
        if let Some(character) = reference
            .resolve_char_ref()
            .map_err(|error| self.error_at(offset, error.to_string()))?
        {
            if !is_xml_char(character) {
                return Err(self.error_at(
                    offset,
                    format!(
                        "character reference resolves to an invalid XML character U+{:04X}",
                        character as u32
                    ),
                ));
            }
            return Ok(character.to_string());
        }

        match &**reference {
            b"amp" => Ok("&".to_owned()),
            b"lt" => Ok("<".to_owned()),
            b"gt" => Ok(">".to_owned()),
            b"apos" => Ok("'".to_owned()),
            b"quot" => Ok("\"".to_owned()),
            name => Err(self.error_at(
                offset,
                format!(
                    "undefined XML entity reference `&{};`",
                    String::from_utf8_lossy(name)
                ),
            )),
        }
    }
}

fn split_name<'a>(
    name: &'a str,
    offset: usize,
    parser: &XmlParser<'_>,
) -> Result<(Option<&'a str>, &'a str), Diagnostic> {
    match name.split_once(':') {
        Some((prefix, local))
            if !prefix.is_empty() && !local.is_empty() && !local.contains(':') =>
        {
            Ok((Some(prefix), local))
        }
        Some(_) => Err(parser.error_at(offset, format!("invalid XML qualified name `{name}`"))),
        None if !name.is_empty() => Ok((None, name)),
        None => Err(parser.error_at(offset, "empty XML element or attribute name")),
    }
}

fn validate_namespace_binding(
    parser: &XmlParser<'_>,
    offset: usize,
    prefix: Option<&str>,
    namespace: &str,
) -> Result<(), Diagnostic> {
    const XML_NAMESPACE: &str = "http://www.w3.org/XML/1998/namespace";
    const XMLNS_NAMESPACE: &str = "http://www.w3.org/2000/xmlns/";

    match prefix {
        None if namespace == XML_NAMESPACE || namespace == XMLNS_NAMESPACE => Err(parser.error_at(
            offset,
            "reserved XML namespace URI may only be bound to its reserved prefix",
        )),
        Some("xml") if namespace != XML_NAMESPACE => Err(parser.error_at(
            offset,
            "the xml prefix must be bound to http://www.w3.org/XML/1998/namespace",
        )),
        Some("xmlns") => Err(parser.error_at(offset, "the xmlns prefix is reserved")),
        Some(_) if namespace.is_empty() => Err(parser.error_at(
            offset,
            "prefixed XML namespaces cannot be undeclared in XML 1.0",
        )),
        Some(_) if namespace == XML_NAMESPACE || namespace == XMLNS_NAMESPACE => Err(parser
            .error_at(
                offset,
                "reserved XML namespace URI may only be bound to its reserved prefix",
            )),
        _ => Ok(()),
    }
}

fn permits_text(element: &Element) -> bool {
    element.namespace.as_deref() != Some(SCXML_NAMESPACE)
        || matches!(
            element.local_name.as_str(),
            "assign" | "content" | "data" | "script"
        )
}

fn is_xml_whitespace(text: &str) -> bool {
    text.chars()
        .all(|character| matches!(character, ' ' | '\t' | '\n' | '\r'))
}

fn is_xml_char(character: char) -> bool {
    matches!(
        character as u32,
        0x9 | 0xA | 0xD | 0x20..=0xD7FF | 0xE000..=0xFFFD | 0x10000..=0x10FFFF
    )
}

fn decode_root(element: &Element, source_name: &str, source: &str) -> Result<Scxml, Diagnostic> {
    require_scxml(
        element,
        source_name,
        source,
        "the root element must be SCXML",
    )?;
    let mut children = Vec::new();
    for child in &element.children {
        if !is_scxml(child) {
            children.push(RootChild::Extension(extension_from_element(child)));
            continue;
        }
        let item = match child.local_name.as_str() {
            "datamodel" => RootChild::DataModel(decode_datamodel(child, source_name, source)?),
            "state" => RootChild::State(decode_state(child, source_name, source)?),
            "parallel" => RootChild::Parallel(decode_parallel(child, source_name, source)?),
            "final" => RootChild::Final(decode_final(child, source_name, source)?),
            "script" => RootChild::Script(decode_script(child, source_name, source)?),
            _ => return Err(unsupported_scxml(source_name, source, child, "scxml")),
        };
        children.push(item);
    }
    Ok(Scxml {
        span: element.span,
        name: attribute(element, "name").map(str::to_owned),
        domain: namespaced_attribute(element, super::BEVY_STATE_TREE_NAMESPACE, "domain")
            .map(str::to_owned),
        version: attribute(element, "version").map(str::to_owned),
        initial: idrefs(attribute(element, "initial")),
        initial_raw: attribute(element, "initial").map(str::to_owned),
        binding: attribute(element, "binding").map(str::to_owned),
        datamodel: attribute(element, "datamodel").map(str::to_owned),
        attributes: element.attributes.clone(),
        children,
    })
}

fn decode_state(element: &Element, source_name: &str, source: &str) -> Result<State, Diagnostic> {
    let mut children = Vec::new();
    for child in &element.children {
        if !is_scxml(child) {
            children.push(StateChild::Extension(extension_from_element(child)));
            continue;
        }
        let item = match child.local_name.as_str() {
            "onentry" => StateChild::OnEntry(decode_onentry(child, source_name, source)?),
            "onexit" => StateChild::OnExit(decode_onexit(child, source_name, source)?),
            "initial" => StateChild::Initial(decode_initial(child, source_name, source)?),
            "state" => StateChild::State(decode_state(child, source_name, source)?),
            "parallel" => StateChild::Parallel(decode_parallel(child, source_name, source)?),
            "final" => StateChild::Final(decode_final(child, source_name, source)?),
            "history" => StateChild::History(decode_history(child, source_name, source)?),
            "transition" => StateChild::Transition(decode_transition(child, source_name, source)?),
            "datamodel" => StateChild::DataModel(decode_datamodel(child, source_name, source)?),
            _ => return Err(unsupported_scxml(source_name, source, child, "state")),
        };
        children.push(item);
    }
    Ok(State {
        span: element.span,
        id: attribute(element, "id").map(str::to_owned),
        initial: idrefs(attribute(element, "initial")),
        initial_raw: attribute(element, "initial").map(str::to_owned),
        attributes: element.attributes.clone(),
        children,
    })
}

fn decode_parallel(
    element: &Element,
    source_name: &str,
    source: &str,
) -> Result<Parallel, Diagnostic> {
    let mut children = Vec::new();
    for child in &element.children {
        if !is_scxml(child) {
            children.push(ParallelChild::Extension(extension_from_element(child)));
            continue;
        }
        let item = match child.local_name.as_str() {
            "onentry" => ParallelChild::OnEntry(decode_onentry(child, source_name, source)?),
            "onexit" => ParallelChild::OnExit(decode_onexit(child, source_name, source)?),
            "state" => ParallelChild::State(decode_state(child, source_name, source)?),
            "parallel" => ParallelChild::Parallel(decode_parallel(child, source_name, source)?),
            "history" => ParallelChild::History(decode_history(child, source_name, source)?),
            "transition" => {
                ParallelChild::Transition(decode_transition(child, source_name, source)?)
            }
            "datamodel" => ParallelChild::DataModel(decode_datamodel(child, source_name, source)?),
            _ => return Err(unsupported_scxml(source_name, source, child, "parallel")),
        };
        children.push(item);
    }
    Ok(Parallel {
        span: element.span,
        id: attribute(element, "id").map(str::to_owned),
        attributes: element.attributes.clone(),
        children,
    })
}

fn decode_final(element: &Element, source_name: &str, source: &str) -> Result<Final, Diagnostic> {
    let mut children = Vec::new();
    for child in &element.children {
        if !is_scxml(child) {
            children.push(FinalChild::Extension(extension_from_element(child)));
            continue;
        }
        let item = match child.local_name.as_str() {
            "onentry" => FinalChild::OnEntry(decode_onentry(child, source_name, source)?),
            "onexit" => FinalChild::OnExit(decode_onexit(child, source_name, source)?),
            "donedata" => FinalChild::DoneData(decode_donedata(child, source_name, source)?),
            _ => return Err(unsupported_scxml(source_name, source, child, "final")),
        };
        children.push(item);
    }
    Ok(Final {
        span: element.span,
        id: attribute(element, "id").map(str::to_owned),
        attributes: element.attributes.clone(),
        children,
    })
}

fn decode_history(
    element: &Element,
    source_name: &str,
    source: &str,
) -> Result<History, Diagnostic> {
    if let Some(value) = attribute(element, "type")
        && !matches!(value, "shallow" | "deep")
    {
        return Err(error_at(
            source_name,
            source,
            element.span.start,
            format!("invalid history type `{value}`"),
        ));
    }
    let mut children = Vec::new();
    for child in &element.children {
        if !is_scxml(child) {
            children.push(HistoryChild::Extension(extension_from_element(child)));
            continue;
        }
        match child.local_name.as_str() {
            "transition" => children.push(HistoryChild::Transition(decode_transition(
                child,
                source_name,
                source,
            )?)),
            _ => return Err(unsupported_scxml(source_name, source, child, "history")),
        }
    }
    Ok(History {
        span: element.span,
        id: attribute(element, "id").map(str::to_owned),
        history_type: attribute(element, "type").map(str::to_owned),
        attributes: element.attributes.clone(),
        children,
    })
}

fn decode_initial(
    element: &Element,
    source_name: &str,
    source: &str,
) -> Result<Initial, Diagnostic> {
    let mut transition = None;
    let mut extensions = Vec::new();
    for child in &element.children {
        if !is_scxml(child) {
            extensions.push(extension_from_element(child));
        } else if child.local_name == "transition" && transition.is_none() {
            transition = Some(decode_transition(child, source_name, source)?);
        } else {
            return Err(unsupported_scxml(source_name, source, child, "initial"));
        }
    }
    Ok(Initial {
        span: element.span,
        attributes: element.attributes.clone(),
        transition,
        extensions,
    })
}

fn decode_datamodel(
    element: &Element,
    source_name: &str,
    source: &str,
) -> Result<DataModel, Diagnostic> {
    let mut children = Vec::new();
    for child in &element.children {
        if !is_scxml(child) {
            children.push(DataModelChild::Extension(extension_from_element(child)));
        } else if child.local_name == "data" {
            children.push(DataModelChild::Data(decode_data(child)));
        } else {
            return Err(unsupported_scxml(source_name, source, child, "datamodel"));
        }
    }
    Ok(DataModel {
        span: element.span,
        attributes: element.attributes.clone(),
        children,
    })
}

fn decode_data(element: &Element) -> Data {
    Data {
        span: element.span,
        id: attribute(element, "id").map(str::to_owned),
        expression: attribute(element, "expr").map(str::to_owned),
        source: attribute(element, "src").map(str::to_owned),
        text: element.text.clone(),
        content: decode_inline_content(element),
        attributes: element.attributes.clone(),
    }
}

fn decode_onentry(
    element: &Element,
    source_name: &str,
    source: &str,
) -> Result<OnEntry, Diagnostic> {
    Ok(OnEntry {
        span: element.span,
        attributes: element.attributes.clone(),
        content: decode_executable_children(element, source_name, source)?,
    })
}

fn decode_onexit(element: &Element, source_name: &str, source: &str) -> Result<OnExit, Diagnostic> {
    Ok(OnExit {
        span: element.span,
        attributes: element.attributes.clone(),
        content: decode_executable_children(element, source_name, source)?,
    })
}

fn decode_transition(
    element: &Element,
    source_name: &str,
    source: &str,
) -> Result<Transition, Diagnostic> {
    let transition_type = match attribute(element, "type") {
        None => None,
        Some("internal") => Some(TransitionType::Internal),
        Some("external") => Some(TransitionType::External),
        Some(value) => {
            return Err(error_at(
                source_name,
                source,
                element.span.start,
                format!("invalid transition type `{value}`"),
            ));
        }
    };
    let event = attribute(element, "event").map(str::to_owned);
    let after = namespaced_attribute(element, super::BEVY_STATE_TREE_NAMESPACE, "after")
        .map(|value| parse_duration(value, element.span, source_name, source))
        .transpose()?;
    Ok(Transition {
        span: element.span,
        events: event
            .as_deref()
            .map(|value| value.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default(),
        event,
        after,
        condition: attribute(element, "cond").map(str::to_owned),
        targets: attribute(element, "target")
            .map(|target| target.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default(),
        transition_type,
        attributes: element.attributes.clone(),
        content: decode_executable_children(element, source_name, source)?,
    })
}

fn decode_content(element: &Element) -> Content {
    decode_content_with_expression(
        element,
        attribute(element, "expr").map(str::to_owned),
        element.attributes.clone(),
    )
}

fn decode_inline_content(element: &Element) -> Option<Content> {
    (!element.content.is_empty()).then(|| decode_content_with_expression(element, None, Vec::new()))
}

fn decode_content_with_expression(
    element: &Element,
    expression: Option<String>,
    attributes: Vec<Attribute>,
) -> Content {
    let parts = element
        .content
        .iter()
        .map(|part| match part {
            RawContent::Text(text) => ContentPart::Text(text.clone()),
            RawContent::Element(child) => ContentPart::Element(payload_from_element(child)),
        })
        .collect();
    Content {
        span: element.span,
        expression,
        attributes,
        parts,
        text: element.text.clone(),
    }
}

fn payload_from_element(element: &Element) -> PayloadElement {
    let content = element
        .content
        .iter()
        .map(|part| match part {
            RawContent::Text(text) => PayloadPart::Text(text.clone()),
            RawContent::Element(child) => PayloadPart::Element(payload_from_element(child)),
        })
        .collect();
    PayloadElement {
        span: element.span,
        name: element.name.clone(),
        local_name: element.local_name.clone(),
        namespace: element.namespace.clone(),
        attributes: element.attributes.clone(),
        content,
        text: element.text.clone(),
    }
}

fn decode_param(element: &Element, source_name: &str, source: &str) -> Result<Param, Diagnostic> {
    let extensions = element
        .children
        .iter()
        .map(|child| {
            if is_scxml(child) {
                Err(unsupported_scxml(source_name, source, child, "param"))
            } else {
                Ok(extension_from_element(child))
            }
        })
        .collect::<Result<_, _>>()?;
    Ok(Param {
        span: element.span,
        name: attribute(element, "name").map(str::to_owned),
        expression: attribute(element, "expr").map(str::to_owned),
        location: attribute(element, "location").map(str::to_owned),
        attributes: element.attributes.clone(),
        extensions,
    })
}

fn decode_donedata(
    element: &Element,
    source_name: &str,
    source: &str,
) -> Result<DoneData, Diagnostic> {
    let mut children = Vec::new();
    for child in &element.children {
        if !is_scxml(child) {
            children.push(DoneDataChild::Extension(extension_from_element(child)));
            continue;
        }
        match child.local_name.as_str() {
            "param" => children.push(DoneDataChild::Param(decode_param(
                child,
                source_name,
                source,
            )?)),
            "content" => children.push(DoneDataChild::Content(decode_content(child))),
            _ => return Err(unsupported_scxml(source_name, source, child, "donedata")),
        }
    }
    Ok(DoneData {
        span: element.span,
        attributes: element.attributes.clone(),
        children,
    })
}

fn decode_executable_children(
    element: &Element,
    source_name: &str,
    source: &str,
) -> Result<Vec<ExecutableContent>, Diagnostic> {
    element
        .children
        .iter()
        .map(|child| decode_executable(child, source_name, source))
        .collect()
}

fn decode_executable(
    element: &Element,
    source_name: &str,
    source: &str,
) -> Result<ExecutableContent, Diagnostic> {
    if !is_scxml(element) {
        return Ok(ExecutableContent::Extension(extension_from_element(
            element,
        )));
    }
    match element.local_name.as_str() {
        "raise" => Ok(ExecutableContent::Raise(Raise {
            span: element.span,
            event: attribute(element, "event").unwrap_or_default().to_owned(),
            attributes: element.attributes.clone(),
        })),
        "assign" => Ok(ExecutableContent::Assign(Assign {
            span: element.span,
            location: attribute(element, "location").map(str::to_owned),
            expression: attribute(element, "expr").map(str::to_owned),
            source: attribute(element, "src").map(str::to_owned),
            content: decode_inline_content(element),
            attributes: element.attributes.clone(),
        })),
        "if" => Ok(ExecutableContent::If(decode_if(
            element,
            source_name,
            source,
        )?)),
        "foreach" => Ok(ExecutableContent::Foreach(decode_foreach(
            element,
            source_name,
            source,
        )?)),
        "log" => Ok(ExecutableContent::Log(Log {
            span: element.span,
            label: attribute(element, "label").map(str::to_owned),
            expression: attribute(element, "expr").map(str::to_owned),
            attributes: element.attributes.clone(),
        })),
        "send" => decode_send(element, source_name, source),
        "cancel" => Ok(ExecutableContent::Cancel(Cancel {
            span: element.span,
            send_id: attribute(element, "sendid").map(str::to_owned),
            send_id_expression: attribute(element, "sendidexpr").map(str::to_owned),
            attributes: element.attributes.clone(),
        })),
        "script" => Ok(ExecutableContent::Script(decode_script(
            element,
            source_name,
            source,
        )?)),
        _ => Err(unsupported_scxml(
            source_name,
            source,
            element,
            "executable content",
        )),
    }
}

fn decode_if(element: &Element, source_name: &str, source: &str) -> Result<If, Diagnostic> {
    let mut children = Vec::new();
    let mut else_started = false;
    for child in &element.children {
        if is_scxml(child) && child.local_name == "elseif" {
            if else_started {
                return Err(error_at(
                    source_name,
                    source,
                    child.span.start,
                    "elseif must precede else",
                ));
            }
            children.push(IfChild::ElseIf(decode_elseif(child, source_name, source)?));
        } else if is_scxml(child) && child.local_name == "else" {
            if else_started {
                return Err(error_at(
                    source_name,
                    source,
                    child.span.start,
                    "an if may contain only one else",
                ));
            }
            else_started = true;
            children.push(IfChild::Else(decode_else(child, source_name, source)?));
        } else {
            let executable = decode_executable(child, source_name, source)?;
            match children.last_mut() {
                Some(IfChild::ElseIf(branch)) => branch.content.push(executable),
                Some(IfChild::Else(branch)) => branch.content.push(executable),
                _ => children.push(IfChild::Executable(executable)),
            }
        }
    }
    Ok(If {
        span: element.span,
        condition: attribute(element, "cond").map(str::to_owned),
        attributes: element.attributes.clone(),
        children,
    })
}

fn decode_elseif(element: &Element, source_name: &str, source: &str) -> Result<ElseIf, Diagnostic> {
    Ok(ElseIf {
        span: element.span,
        condition: attribute(element, "cond").map(str::to_owned),
        attributes: element.attributes.clone(),
        content: decode_executable_children(element, source_name, source)?,
    })
}

fn decode_else(element: &Element, source_name: &str, source: &str) -> Result<Else, Diagnostic> {
    Ok(Else {
        span: element.span,
        attributes: element.attributes.clone(),
        content: decode_executable_children(element, source_name, source)?,
    })
}

fn decode_foreach(
    element: &Element,
    source_name: &str,
    source: &str,
) -> Result<Foreach, Diagnostic> {
    Ok(Foreach {
        span: element.span,
        array: attribute(element, "array").map(str::to_owned),
        item: attribute(element, "item").map(str::to_owned),
        index: attribute(element, "index").map(str::to_owned),
        attributes: element.attributes.clone(),
        content: decode_executable_children(element, source_name, source)?,
    })
}

fn decode_send(
    element: &Element,
    source_name: &str,
    source: &str,
) -> Result<ExecutableContent, Diagnostic> {
    let mut children = Vec::new();
    for child in &element.children {
        if !is_scxml(child) {
            children.push(SendChild::Extension(extension_from_element(child)));
            continue;
        }
        match child.local_name.as_str() {
            "param" => children.push(SendChild::Param(decode_param(child, source_name, source)?)),
            "content" => children.push(SendChild::Content(decode_content(child))),
            _ => return Err(unsupported_scxml(source_name, source, child, "send")),
        }
    }
    Ok(ExecutableContent::Send(Send {
        span: element.span,
        event: attribute(element, "event").map(str::to_owned),
        event_expression: attribute(element, "eventexpr").map(str::to_owned),
        target: attribute(element, "target").map(str::to_owned),
        target_expression: attribute(element, "targetexpr").map(str::to_owned),
        send_type: attribute(element, "type").map(str::to_owned),
        type_expression: attribute(element, "typeexpr").map(str::to_owned),
        id: attribute(element, "id").map(str::to_owned),
        id_location: attribute(element, "idlocation").map(str::to_owned),
        delay: attribute(element, "delay").map(str::to_owned),
        delay_expression: attribute(element, "delayexpr").map(str::to_owned),
        namelist: attribute(element, "namelist").map(str::to_owned),
        attributes: element.attributes.clone(),
        children,
    }))
}

fn decode_script(element: &Element, source_name: &str, source: &str) -> Result<Script, Diagnostic> {
    let mut extensions = Vec::new();
    for child in &element.children {
        if is_scxml(child) {
            return Err(unsupported_scxml(source_name, source, child, "script"));
        }
        extensions.push(extension_from_element(child));
    }
    Ok(Script {
        span: element.span,
        source: attribute(element, "src").map(str::to_owned),
        text: element.text.clone(),
        attributes: element.attributes.clone(),
        extensions,
    })
}

fn attribute<'a>(element: &'a Element, name: &str) -> Option<&'a str> {
    element
        .attributes
        .iter()
        .find(|attribute| attribute.name == name && attribute.namespace.is_none())
        .map(|attribute| attribute.value.as_str())
}

fn namespaced_attribute<'a>(element: &'a Element, namespace: &str, name: &str) -> Option<&'a str> {
    element
        .attributes
        .iter()
        .find(|attribute| {
            attribute.namespace.as_deref() == Some(namespace) && attribute.local_name == name
        })
        .map(|attribute| attribute.value.as_str())
}

fn parse_duration(
    value: &str,
    span: Span,
    source_name: &str,
    source: &str,
) -> Result<Duration, Diagnostic> {
    let (number, unit_nanos, fractional_digits) = value
        .strip_suffix("ms")
        .map(|number| (number, 1_000_000_u128, 6))
        .or_else(|| {
            value
                .strip_suffix('s')
                .map(|number| (number, 1_000_000_000_u128, 9))
        })
        .ok_or_else(|| invalid_after_duration(source_name, source, span))?;
    let mut parts = number.split('.');
    let integer = parts.next().unwrap_or_default();
    let fraction = parts.next();
    let valid_number = !integer.is_empty()
        && integer.bytes().all(|byte| byte.is_ascii_digit())
        && fraction.is_none_or(|fraction| {
            !fraction.is_empty() && fraction.bytes().all(|byte| byte.is_ascii_digit())
        })
        && parts.next().is_none();
    if !valid_number {
        return Err(invalid_after_duration(source_name, source, span));
    }
    let whole_nanos = integer
        .parse::<u128>()
        .ok()
        .and_then(|integer| integer.checked_mul(unit_nanos))
        .ok_or_else(|| invalid_after_duration(source_name, source, span))?;
    let fraction_nanos = match fraction {
        Some(fraction) => decimal_fraction_nanos(fraction, fractional_digits)
            .ok_or_else(|| invalid_after_duration(source_name, source, span))?,
        None => 0,
    };
    let total_nanos = whole_nanos
        .checked_add(fraction_nanos)
        .filter(|total| *total <= u128::from(u64::MAX) * 1_000_000_000 + 999_999_999)
        .ok_or_else(|| invalid_after_duration(source_name, source, span))?;
    let duration = Duration::new(
        (total_nanos / 1_000_000_000) as u64,
        (total_nanos % 1_000_000_000) as u32,
    );
    if duration.is_zero() {
        return Err(invalid_after_duration(source_name, source, span));
    }
    Ok(duration)
}

fn decimal_fraction_nanos(fraction: &str, precision: usize) -> Option<u128> {
    let mut nanos = 0_u128;
    for (index, byte) in fraction.bytes().enumerate() {
        let digit = u128::from(byte - b'0');
        if index < precision {
            nanos += digit * 10_u128.pow((precision - index - 1) as u32);
        } else if digit != 0 {
            return None;
        }
    }
    Some(nanos)
}

fn invalid_after_duration(source_name: &str, source: &str, span: Span) -> Diagnostic {
    error_at(
        source_name,
        source,
        span.start,
        "bst:after must be a positive duration literal ending in `ms` or `s`",
    )
}

fn idrefs(value: Option<&str>) -> Vec<String> {
    value
        .into_iter()
        .flat_map(str::split_whitespace)
        .map(str::to_owned)
        .collect()
}

fn is_scxml(element: &Element) -> bool {
    element.namespace.as_deref() == Some(SCXML_NAMESPACE)
}

fn validate_known_scxml_names(
    element: &Element,
    source_name: &str,
    source: &str,
) -> Result<(), Diagnostic> {
    if is_scxml(element)
        && !matches!(
            element.local_name.as_str(),
            "scxml"
                | "onentry"
                | "onexit"
                | "initial"
                | "datamodel"
                | "data"
                | "state"
                | "parallel"
                | "final"
                | "history"
                | "transition"
                | "param"
                | "donedata"
                | "content"
                | "raise"
                | "assign"
                | "if"
                | "elseif"
                | "else"
                | "foreach"
                | "log"
                | "send"
                | "cancel"
                | "script"
        )
    {
        return Err(unsupported_scxml(source_name, source, element, "document"));
    }
    // Payload XML is opaque data, not authored SCXML syntax.
    if is_scxml(element) && matches!(element.local_name.as_str(), "content" | "data" | "assign") {
        return Ok(());
    }
    for child in &element.children {
        validate_known_scxml_names(child, source_name, source)?;
    }
    Ok(())
}

fn require_scxml(
    element: &Element,
    source_name: &str,
    source: &str,
    message: &str,
) -> Result<(), Diagnostic> {
    if is_scxml(element) && element.local_name == "scxml" {
        Ok(())
    } else {
        Err(error_at(source_name, source, element.span.start, message))
    }
}

fn unsupported_scxml(
    source_name: &str,
    source: &str,
    element: &Element,
    parent: &str,
) -> Diagnostic {
    error_at(
        source_name,
        source,
        element.span.start,
        format!(
            "unsupported SCXML element <{}> in <{}>",
            element.local_name, parent
        ),
    )
}

fn extension_from_element(element: &Element) -> Extension {
    let content = element
        .content
        .iter()
        .map(|content| match content {
            RawContent::Text(text) => ExtensionContent::Text(text.clone()),
            RawContent::Element(child) => ExtensionContent::Element(extension_from_element(child)),
        })
        .collect();
    Extension {
        span: element.span,
        name: element.name.clone(),
        local_name: element.local_name.clone(),
        namespace: element.namespace.clone(),
        attributes: element.attributes.clone(),
        content,
        text: element.text.clone(),
    }
}

fn error_at(
    source_name: &str,
    source: &str,
    offset: usize,
    message: impl Into<String>,
) -> Diagnostic {
    diagnostic_at(source_name, source, offset, message.into())
}

fn diagnostic_at(source_name: &str, source: &str, offset: usize, message: String) -> Diagnostic {
    let offset = offset.min(source.len());
    let prefix = &source[..offset];
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
    let column = prefix
        .rsplit_once('\n')
        .map_or(prefix.chars().count() + 1, |(_, current)| {
            current.chars().count() + 1
        });
    Diagnostic {
        source_name: source_name.to_owned(),
        message,
        span: Span {
            start: offset,
            end: offset,
        },
        line,
        column,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NS: &str = "http://www.w3.org/2005/07/scxml";

    #[test]
    fn resolves_the_authored_domain_by_namespace() {
        let source = format!(
            r#"<scxml xmlns="{NS}" xmlns:state-tree="{}" state-tree:domain="simulation"><state/></scxml>"#,
            super::super::BEVY_STATE_TREE_NAMESPACE,
        );
        let document = parse("domain.scxml", &source).expect("valid domain declaration");

        assert_eq!(document.root.domain.as_deref(), Some("simulation"));
    }

    #[test]
    fn parses_namespaces_order_and_authored_transition_tokens() {
        let source = format!(
            "<s:scxml xmlns:s=\"{NS}\" initial=\"main\">\n  <s:datamodel><s:data id=\"x\" expr=\"a + b\"/></s:datamodel>\n  <s:state id=\"main\"><s:onentry><s:raise event=\"started\"/></s:onentry><s:transition event=\"go stop\" target=\"a b\" type=\"internal\"/></s:state>\n</s:scxml>"
        );
        let document = parse("states.scxml", &source).expect("valid SCXML");
        assert_eq!(document.source_name, "states.scxml");
        assert_eq!(document.root.initial, ["main"]);
        assert_eq!(document.root.children.len(), 2);
        let state = match &document.root.children[1] {
            RootChild::State(state) => state,
            other => panic!("expected state, got {other:?}"),
        };
        let transition = state
            .children
            .iter()
            .find_map(|child| match child {
                StateChild::Transition(transition) => Some(transition),
                _ => None,
            })
            .expect("transition");
        assert_eq!(transition.event.as_deref(), Some("go stop"));
        assert_eq!(transition.events, ["go", "stop"]);
        assert_eq!(transition.targets, ["a", "b"]);
        assert_eq!(transition.transition_type, Some(TransitionType::Internal));
        assert!(transition.span.end > transition.span.start);
    }

    #[test]
    fn parses_all_requested_executable_forms_and_preserves_branch_order() {
        let source = format!(
            r#"<scxml xmlns="{NS}"><state id="s"><onentry>
              <raise event="wake"/><assign location="ctx.value" expr="raw + 1"/>
              <if cond="ready"><log label="a" expr="value"/><elseif cond="later"><foreach array="items" item="item"><send event="tick"/></foreach></elseif><else><cancel sendid="timer"/><script>if (x &lt; 2) {{ x++; }}</script></else></if>
            </onentry></state></scxml>"#
        );
        let document = parse("exec.scxml", &source).expect("valid executable content");
        let state = match &document.root.children[0] {
            RootChild::State(state) => state,
            _ => panic!("state"),
        };
        let entry = match &state.children[0] {
            StateChild::OnEntry(entry) => entry,
            _ => panic!("onentry"),
        };
        assert!(matches!(entry.content[0], ExecutableContent::Raise(_)));
        assert!(matches!(entry.content[1], ExecutableContent::Assign(_)));
        let conditional = match &entry.content[2] {
            ExecutableContent::If(value) => value,
            _ => panic!("if"),
        };
        assert!(matches!(conditional.children[0], IfChild::Executable(_)));
        assert!(matches!(conditional.children[1], IfChild::ElseIf(_)));
        assert!(matches!(conditional.children[2], IfChild::Else(_)));
        let else_branch = match &conditional.children[2] {
            IfChild::Else(value) => value,
            _ => unreachable!(),
        };
        let script = match &else_branch.content[1] {
            ExecutableContent::Script(value) => value,
            _ => panic!("script"),
        };
        assert!(script.text.contains("x++"));
    }

    #[test]
    fn retains_foreign_elements_and_resolves_their_namespace_alias() {
        let source = format!(
            "<scxml xmlns=\"{NS}\" xmlns:game=\"urn:example:game\"><state id=\"s\"><game:meta value=\"1\">before<game:child>ok</game:child>after</game:meta></state></scxml>"
        );
        let document = parse("foreign.scxml", &source).expect("foreign extension");
        let state = match &document.root.children[0] {
            RootChild::State(state) => state,
            _ => panic!("state"),
        };
        let extension = match &state.children[0] {
            StateChild::Extension(extension) => extension,
            _ => panic!("extension"),
        };
        assert_eq!(extension.name, "game:meta");
        assert_eq!(extension.namespace.as_deref(), Some("urn:example:game"));
        assert_eq!(extension.text, "beforeafter");
        assert!(matches!(extension.content[0], ExtensionContent::Text(_)));
        assert!(matches!(extension.content[1], ExtensionContent::Element(_)));
        assert!(matches!(extension.content[2], ExtensionContent::Text(_)));
    }

    #[test]
    fn rejects_unknown_scxml_elements_with_precise_location() {
        let source = format!("<scxml xmlns=\"{NS}\">\n  <mystery/>\n</scxml>");
        let error = parse("bad.scxml", &source).expect_err("unknown SCXML must fail");
        assert_eq!(error.source_name, "bad.scxml");
        assert_eq!(error.line, 2);
        assert_eq!(error.column, 3);
        assert_eq!(error.span.start, source.find("<mystery").unwrap());
        assert!(
            error
                .message
                .contains("unsupported SCXML element <mystery>")
        );
    }

    #[test]
    fn rejects_state_invoke_with_the_generic_unknown_element_diagnostic() {
        let unknown = parse(
            "unknown.scxml",
            &format!("<scxml xmlns=\"{NS}\"><state><mystery/></state></scxml>"),
        )
        .expect_err("unknown SCXML must fail");
        let invoke = parse(
            "invoke.scxml",
            &format!("<scxml xmlns=\"{NS}\"><state><invoke/></state></scxml>"),
        )
        .expect_err("invoke must be treated as unknown SCXML");

        assert_eq!(invoke.line, unknown.line);
        assert_eq!(invoke.column, unknown.column);
        assert_eq!(
            invoke.message.replace("<invoke>", "<mystery>"),
            unknown.message
        );
    }

    #[test]
    fn rejects_non_scxml_root_even_when_the_local_name_matches() {
        let source = "<scxml xmlns=\"urn:not-scxml\"/>";
        let error = parse("wrong-root.scxml", source).expect_err("namespace is required");
        assert_eq!(error.line, 1);
        assert!(error.message.contains("root element must be SCXML"));
    }

    #[test]
    fn accepts_targetless_and_eventless_transitions() {
        let source = format!(
            "<scxml xmlns=\"{NS}\"><state id=\"s\"><transition cond=\"ready\"/><transition event=\"done\"/></state></scxml>"
        );
        let document = parse("transitions.scxml", &source).expect("targetless transitions");
        let state = match &document.root.children[0] {
            RootChild::State(state) => state,
            _ => panic!("state"),
        };
        assert!(matches!(state.children[0], StateChild::Transition(_)));
        let transition = match &state.children[1] {
            StateChild::Transition(value) => value,
            _ => unreachable!(),
        };
        assert_eq!(transition.targets, Vec::<String>::new());
        assert_eq!(transition.event.as_deref(), Some("done"));
    }

    #[test]
    fn parses_namespaced_after_durations_without_classifying_them_as_eventless() {
        let source = format!(
            r#"<scxml xmlns="{NS}" xmlns:timer="{}"><state><transition timer:after="1.5s" target="done"/></state><final id="done"/></scxml>"#,
            super::super::BEVY_STATE_TREE_NAMESPACE,
        );
        let document = parse("after.scxml", &source).expect("valid after transition");
        let state = match &document.root.children[0] {
            RootChild::State(state) => state,
            _ => panic!("state"),
        };
        let transition = match &state.children[0] {
            StateChild::Transition(transition) => transition,
            _ => panic!("transition"),
        };

        assert_eq!(transition.after, Some(Duration::from_millis(1_500)));
        assert!(
            document
                .capabilities()
                .contains(&Capability::AfterTransition)
        );
        assert!(
            !document
                .capabilities()
                .contains(&Capability::EventlessTransition)
        );
    }

    #[test]
    fn rejects_malformed_after_duration_literals() {
        let namespace = super::super::BEVY_STATE_TREE_NAMESPACE;
        for value in [
            "0s",
            "0.0ms",
            "-1s",
            ".5s",
            "1.s",
            "1e3s",
            "NaNs",
            "infs",
            "1 s",
            " 1s",
            "1m",
            "0.0000000001s",
            "18446744073709551616s",
        ] {
            let source = format!(
                r#"<scxml xmlns="{NS}" xmlns:bst="{namespace}"><state><transition bst:after="{value}" target="done"/></state><final id="done"/></scxml>"#,
            );
            let error = parse("invalid-after.scxml", &source).unwrap_err();
            assert!(
                error
                    .message
                    .contains("bst:after must be a positive duration literal"),
                "unexpected diagnostic for {value:?}: {error}"
            );
        }
    }

    #[test]
    fn accepts_the_largest_representable_after_duration() {
        let source = format!(
            r#"<scxml xmlns="{NS}" xmlns:bst="{}"><state><transition bst:after="18446744073709551615.999999999s" target="done"/></state><final id="done"/></scxml>"#,
            super::super::BEVY_STATE_TREE_NAMESPACE,
        );

        parse("maximum-after.scxml", &source).expect("representable duration");
    }

    #[test]
    fn decodes_parallel_history_initial_and_final_nodes() {
        let source = format!(
            r#"<scxml xmlns="{NS}"><parallel id="regions"><state id="region"><history id="remember" type="deep"><transition target="region"/></history><initial><transition target="region"/></initial><final id="finished"><onexit><log expr="done"/></onexit></final></state></parallel></scxml>"#
        );
        let document = parse("nodes.scxml", &source).expect("all authored node forms");
        let parallel = match &document.root.children[0] {
            RootChild::Parallel(value) => value,
            _ => panic!("parallel"),
        };
        let state = match &parallel.children[0] {
            ParallelChild::State(value) => value,
            _ => panic!("region state"),
        };
        assert!(matches!(state.children[0], StateChild::History(_)));
        assert!(matches!(state.children[1], StateChild::Initial(_)));
        assert!(matches!(state.children[2], StateChild::Final(_)));
    }

    #[test]
    fn parses_optional_ids_payload_nodes_local_datamodel_and_sequence_conditionals() {
        let source = format!(
            r#"<scxml xmlns="{NS}" xmlns:x="urn:payload"><script>root script</script><state><datamodel><data expr="raw"/></datamodel><onentry><if cond="first"><raise event="first"/><elseif cond="second"/><log expr="second"/><else/><script>fallback</script></if><send event="message"><param name="one"/><content>body<x:value/></content></send></onentry><final><donedata><param name="result"/><content>done</content></donedata></final></state></scxml>"#
        );
        let document = parse("standard-forms.scxml", &source).expect("valid authored SCXML");
        let root_script = match &document.root.children[0] {
            RootChild::Script(value) => value,
            _ => panic!("top-level script"),
        };
        assert_eq!(root_script.text, "root script");
        let state = match &document.root.children[1] {
            RootChild::State(value) => value,
            _ => panic!("state"),
        };
        assert_eq!(state.id, None);
        assert!(matches!(state.children[0], StateChild::DataModel(_)));
        let entry = match &state.children[1] {
            StateChild::OnEntry(value) => value,
            _ => panic!("onentry"),
        };
        let conditional = match &entry.content[0] {
            ExecutableContent::If(value) => value,
            _ => panic!("if"),
        };
        assert!(matches!(conditional.children[0], IfChild::Executable(_)));
        let elseif = match &conditional.children[1] {
            IfChild::ElseIf(value) => value,
            _ => panic!("elseif marker"),
        };
        assert!(matches!(elseif.content[0], ExecutableContent::Log(_)));
        let else_branch = match &conditional.children[2] {
            IfChild::Else(value) => value,
            _ => panic!("else marker"),
        };
        assert!(matches!(
            else_branch.content[0],
            ExecutableContent::Script(_)
        ));
        let send = match &entry.content[1] {
            ExecutableContent::Send(value) => value,
            _ => panic!("send"),
        };
        assert!(matches!(send.children[0], SendChild::Param(_)));
        let content = match &send.children[1] {
            SendChild::Content(value) => value,
            _ => panic!("send content"),
        };
        assert_eq!(content.text, "body");
        assert!(
            content
                .parts
                .iter()
                .any(|part| matches!(part, ContentPart::Element(_)))
        );
        let final_state = match &state.children[2] {
            StateChild::Final(value) => value,
            _ => panic!("final"),
        };
        let done_data = match &final_state.children[0] {
            FinalChild::DoneData(value) => value,
            _ => panic!("done data"),
        };
        assert!(matches!(done_data.children[0], DoneDataChild::Param(_)));
        assert!(matches!(done_data.children[1], DoneDataChild::Content(_)));
    }

    #[test]
    fn preserves_lists_payloads_and_qualified_attributes() {
        let document = parse(
            "authored-forms.scxml",
            &format!(
                r#"<scxml xmlns="{NS}" xmlns:x="urn:payload" initial="left right">
                  <datamodel><data id="payload">before<x:item>inside</x:item>after</data></datamodel>
                  <state id="left" initial="a b">
                    <initial><transition target="a b"/></initial>
                    <onentry><send event="payload" type="bevy.test"><content><x:message/></content></send></onentry>
                    <state id="a"/><state id="b"/>
                  </state>
                  <state id="right"/>
                </scxml>"#
            ),
        )
        .expect("authored SCXML forms");
        assert_eq!(document.root.initial, ["left", "right"]);
        let model = match &document.root.children[0] {
            RootChild::DataModel(model) => model,
            _ => panic!("datamodel"),
        };
        let data = match &model.children[0] {
            DataModelChild::Data(data) => data,
            _ => panic!("data"),
        };
        let payload = data.content.as_ref().expect("data payload");
        assert!(matches!(payload.parts[0], ContentPart::Text(_)));
        assert!(matches!(payload.parts[1], ContentPart::Element(_)));
        assert!(matches!(payload.parts[2], ContentPart::Text(_)));
        let ContentPart::Element(item) = &payload.parts[1] else {
            unreachable!();
        };
        assert_eq!(item.text, "inside");
        assert!(matches!(item.content[0], PayloadPart::Text(_)));
        let state = match &document.root.children[1] {
            RootChild::State(state) => state,
            _ => panic!("state"),
        };
        assert_eq!(state.initial, ["a", "b"]);
        let initial = match &state.children[0] {
            StateChild::Initial(initial) => initial,
            _ => panic!("initial"),
        };
        assert_eq!(initial.transition.as_ref().unwrap().targets, ["a", "b"]);
        let entry = match &state.children[1] {
            StateChild::OnEntry(entry) => entry,
            _ => panic!("onentry"),
        };
        let send = match &entry.content[0] {
            ExecutableContent::Send(send) => send,
            _ => panic!("send"),
        };
        assert_eq!(send.send_type.as_deref(), Some("bevy.test"));
        let capabilities = document.capabilities();
        assert!(capabilities.contains(&Capability::PayloadContent));
        assert!(capabilities.contains(&Capability::Content));
        assert!(!capabilities.contains(&Capability::ForeignExtension));
    }

    #[test]
    fn rejects_invalid_transition_type_at_the_element() {
        let source = format!(
            "<scxml xmlns=\"{NS}\"><state id=\"s\"><transition type=\"local\"/></state></scxml>"
        );
        let error = parse("type.scxml", &source).expect_err("invalid type");
        assert!(error.message.contains("invalid transition type `local`"));
        assert_eq!(error.span.start, source.find("<transition").unwrap());
    }

    #[test]
    fn enforces_root_and_parallel_child_grammars() {
        for source in [
            format!(r#"<scxml xmlns="{NS}"><onentry/></scxml>"#),
            format!(r#"<scxml xmlns="{NS}"><initial/></scxml>"#),
            format!(r#"<scxml xmlns="{NS}"><parallel><initial/></parallel></scxml>"#),
            format!(r#"<scxml xmlns="{NS}"><parallel><final/></parallel></scxml>"#),
        ] {
            let error = parse("grammar.scxml", &source).expect_err("invalid parent grammar");
            assert!(error.message.contains("unsupported SCXML element"));
        }

        let source = format!(
            r#"<scxml xmlns="{NS}"><parallel><datamodel><data/></datamodel><state/></parallel></scxml>"#
        );
        let document = parse("parallel-datamodel.scxml", &source).expect("parallel datamodel");
        let parallel = match &document.root.children[0] {
            RootChild::Parallel(value) => value,
            _ => panic!("parallel"),
        };
        assert!(matches!(parallel.children[0], ParallelChild::DataModel(_)));
    }

    #[test]
    fn reports_unclosed_elements_at_end_of_file() {
        let source = format!("<scxml xmlns=\"{NS}\">\n  <state id=\"x\">");
        let error = parse("truncated.scxml", &source).expect_err("truncated XML");
        assert_eq!(error.line, 2);
        assert!(error.message.contains("unexpected end of file"));
    }

    #[test]
    fn analyzes_structural_and_executable_capabilities_recursively() {
        let source = format!(
            r#"<scxml xmlns="{NS}" xmlns:x="urn:example:extension">
              <datamodel><data id="value" expr="1"/></datamodel>
              <state id="root">
                <state id="child">
                  <initial><transition target="leaf"/></initial>
                  <history id="shallow"/><history id="deep" type="deep"/>
                  <transition event="*" target="one two"/>
                  <transition event="done.* next" cond="ready" type="internal" target="one"/>
                  <transition/>
                  <onentry>
                    <raise event="raised"/><assign location="value" expr="2"/>
                    <if cond="outer">
                      <foreach array="items" item="item">
                        <if cond="inner"><log expr="item"/></if>
                      </foreach>
                      <elseif cond="alternative"><send event="sent"><x:payload><x:nested/></x:payload></send></elseif>
                      <else><cancel sendid="timer"/><script>value++;</script></else>
                    </if>
                  </onentry>
                </state>
              </state>
              <parallel id="regions"><state id="left"/><state id="right"/></parallel>
              <final id="finished"><onentry><log expr="value"/></onentry></final>
              <x:document-meta><x:child/></x:document-meta>
            </scxml>"#
        );
        let document = parse("capabilities.scxml", &source).expect("valid SCXML");
        let expected = [
            Capability::CompoundState,
            Capability::DefaultInitialTransition,
            Capability::ExplicitInitialTransition,
            Capability::ParallelState,
            Capability::FinalState,
            Capability::ShallowHistory,
            Capability::DeepHistory,
            Capability::EventlessTransition,
            Capability::MultiEventTransition,
            Capability::MultiTargetTransition,
            Capability::TargetlessTransition,
            Capability::InternalTransition,
            Capability::Conditions,
            Capability::WildcardEventDescriptor,
            Capability::PrefixEventDescriptor,
            Capability::Raise,
            Capability::Assign,
            Capability::If,
            Capability::Foreach,
            Capability::Log,
            Capability::Send,
            Capability::Cancel,
            Capability::Script,
            Capability::DataModel,
            Capability::Data,
            Capability::ForeignExtension,
        ]
        .into_iter()
        .collect::<BTreeSet<_>>();

        assert_eq!(document.capabilities(), expected);
        assert_eq!(document.capabilities(), document.capabilities());
    }

    #[test]
    fn analyzes_explicit_initial_attribute_and_event_descriptor_forms() {
        let source = format!(
            r#"<scxml xmlns="{NS}" initial="main">
              <state id="main" initial="leaf"><state id="leaf"/></state>
              <state id="other"><transition event="alpha.*"/></state>
            </scxml>"#
        );
        let document = parse("initials.scxml", &source).expect("valid SCXML");
        let capabilities = document.capabilities();

        assert!(capabilities.contains(&Capability::CompoundState));
        assert!(capabilities.contains(&Capability::ExplicitInitialTransition));
        assert!(!capabilities.contains(&Capability::DefaultInitialTransition));
        assert!(capabilities.contains(&Capability::PrefixEventDescriptor));
        assert!(!capabilities.contains(&Capability::WildcardEventDescriptor));
    }

    #[test]
    fn normalizes_xml_text_resolves_references_and_rejects_dtds() {
        let source = format!(
            "<scxml xmlns=\"{NS}\"><state><onentry><script><![CDATA[first\r\nsecond]]>&amp;&#x41;</script></onentry></state></scxml>"
        );
        let document = parse("xml-content.scxml", &source).expect("valid XML content");
        let state = match &document.root.children[0] {
            RootChild::State(value) => value,
            _ => panic!("state"),
        };
        let onentry = match &state.children[0] {
            StateChild::OnEntry(value) => value,
            _ => panic!("onentry"),
        };
        let script = match &onentry.content[0] {
            ExecutableContent::Script(value) => value,
            _ => panic!("script"),
        };
        assert_eq!(script.text, "first\nsecond&A");

        let undefined = format!(
            r#"<scxml xmlns="{NS}"><state><onentry><script>&unknown;</script></onentry></state></scxml>"#
        );
        let error = parse("undefined-entity.scxml", &undefined).expect_err("undefined entity");
        assert!(error.message.contains("undefined XML entity reference"));

        let doctype = format!(r#"<!DOCTYPE scxml><scxml xmlns="{NS}"/>"#);
        let error = parse("doctype.scxml", &doctype).expect_err("DTD");
        assert!(error.message.contains("DOCTYPE and DTD"));
    }

    #[test]
    fn validates_namespaces_attributes_and_structural_text() {
        let invalid_bindings = [
            format!(r#"<scxml xmlns="{NS}" xmlns:xml="urn:wrong"/>"#),
            format!(r#"<scxml xmlns="{NS}" xmlns:xmlns="urn:wrong"/>"#),
            format!(r#"<scxml xmlns="{NS}" xmlns:p=""/>"#),
        ];
        for source in invalid_bindings {
            parse("namespace.scxml", &source).expect_err("invalid namespace binding");
        }

        let duplicate = format!(
            r#"<scxml xmlns="{NS}" xmlns:a="urn:payload" xmlns:b="urn:payload"><state a:value="1" b:value="2"/></scxml>"#
        );
        let error =
            parse("duplicate-attribute.scxml", &duplicate).expect_err("duplicate attribute");
        assert!(error.message.contains("duplicate XML attribute"));

        let structural_text = format!(r#"<scxml xmlns="{NS}"><state>not allowed</state></scxml>"#);
        let error = parse("structural-text.scxml", &structural_text).expect_err("structural text");
        assert!(error.message.contains("non-whitespace direct text"));

        let payload = format!(
            r#"<scxml xmlns="{NS}"><state><onentry><assign location="value"><nested>kept</nested></assign></onentry></state></scxml>"#
        );
        let document = parse("payload-text.scxml", &payload).expect("payload text");
        let state = match &document.root.children[0] {
            RootChild::State(state) => state,
            _ => panic!("state"),
        };
        let entry = match &state.children[0] {
            StateChild::OnEntry(entry) => entry,
            _ => panic!("onentry"),
        };
        let assign = match &entry.content[0] {
            ExecutableContent::Assign(assign) => assign,
            _ => panic!("assign"),
        };
        assert!(assign.content.is_some());
    }
}
