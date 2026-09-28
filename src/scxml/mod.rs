//! Namespace-aware SCXML source parsing.
//!
//! This module is an authored-syntax frontend. It deliberately does not resolve
//! state ids, evaluate expressions, or build a runtime state tree.

use std::collections::{BTreeSet, HashMap};
use std::fmt;

use quick_xml::events::{BytesRef, BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

/// The namespace URI defined by the SCXML specification.
pub const SCXML_NAMESPACE: &str = "http://www.w3.org/2005/07/scxml";

/// A complete parsed SCXML document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    pub source_name: String,
    pub span: Span,
    pub root: Scxml,
}

/// A structural feature required by an authored SCXML document.
///
/// Capabilities describe syntax present in the AST, not whether that syntax is
/// valid for a particular runtime configuration. The variants and their
/// ordering are stable so a returned [`BTreeSet`] can be compared or serialized
/// deterministically by callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Capability {
    /// A compound state or compound SCXML root is present.
    CompoundState,
    /// A compound state relies on its implicit initial transition.
    DefaultInitialTransition,
    /// An `initial` attribute or explicit `<initial>` element is present.
    ExplicitInitialTransition,
    /// A parallel state is present.
    ParallelState,
    /// A final state is present.
    FinalState,
    /// A shallow history state is present.
    ShallowHistory,
    /// A deep history state is present.
    DeepHistory,
    /// An eventless transition is present.
    EventlessTransition,
    /// A transition has more than one event descriptor.
    MultiEventTransition,
    /// A transition has more than one target.
    MultiTargetTransition,
    /// A targetless transition is present.
    TargetlessTransition,
    /// An internal transition is present.
    InternalTransition,
    /// A transition or executable conditional has a condition.
    Conditions,
    /// A wildcard event descriptor (`*`) is present.
    WildcardEventDescriptor,
    /// A prefix event descriptor (for example, `done.*`) is present.
    PrefixEventDescriptor,
    /// A `<raise>` executable is present.
    Raise,
    /// An `<assign>` executable is present.
    Assign,
    /// An `<if>` executable is present.
    If,
    /// A `<foreach>` executable is present.
    Foreach,
    /// A `<log>` executable is present.
    Log,
    /// A `<send>` executable is present.
    Send,
    /// A `<cancel>` executable is present.
    Cancel,
    /// A `<script>` executable is present.
    Script,
    /// A datamodel container is present.
    DataModel,
    /// A `<data>` item is present.
    Data,
    /// An invocation is present.
    Invoke,
    /// An invocation finalizer is present.
    Finalize,
    /// A standard parameter payload is present.
    Param,
    /// A standard message or done-event content payload is present.
    Content,
    /// Inline XML payload is present in data, assign, or content.
    PayloadContent,
    /// A foreign-namespaced extension is present.
    ForeignExtension,
}

impl Document {
    /// Return the deterministic set of structural capabilities used by this document.
    pub fn capabilities(&self) -> BTreeSet<Capability> {
        let mut capabilities = BTreeSet::new();
        CapabilityAnalyzer::new(&mut capabilities).visit_scxml(&self.root);
        capabilities
    }
}

/// The document element (`scxml`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scxml {
    pub span: Span,
    pub name: Option<String>,
    pub version: Option<String>,
    pub initial: Vec<String>,
    pub initial_raw: Option<String>,
    pub binding: Option<String>,
    pub datamodel: Option<String>,
    pub attributes: Vec<Attribute>,
    pub children: Vec<RootChild>,
}

/// Children which may occur directly below the SCXML document element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootChild {
    DataModel(DataModel),
    State(State),
    Parallel(Parallel),
    Final(Final),
    Script(Script),
    Extension(Extension),
}

/// A compound or atomic SCXML state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    pub span: Span,
    pub id: Option<String>,
    pub initial: Vec<String>,
    pub initial_raw: Option<String>,
    pub attributes: Vec<Attribute>,
    pub children: Vec<StateChild>,
}

/// A parallel state and its regions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parallel {
    pub span: Span,
    pub id: Option<String>,
    pub attributes: Vec<Attribute>,
    pub children: Vec<ParallelChild>,
}

/// Direct children allowed in a parallel state. Parallel states do not have
/// direct initial or final pseudo-state children; their regions are states or
/// nested parallel states.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParallelChild {
    OnEntry(OnEntry),
    OnExit(OnExit),
    State(State),
    Parallel(Parallel),
    History(History),
    Transition(Transition),
    Invoke(Invoke),
    DataModel(DataModel),
    Extension(Extension),
}

/// A final state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Final {
    pub span: Span,
    pub id: Option<String>,
    pub attributes: Vec<Attribute>,
    pub children: Vec<FinalChild>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinalChild {
    OnEntry(OnEntry),
    OnExit(OnExit),
    DoneData(DoneData),
    Extension(Extension),
}

/// A history pseudo-state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct History {
    pub span: Span,
    pub id: Option<String>,
    pub history_type: Option<String>,
    pub attributes: Vec<Attribute>,
    pub children: Vec<HistoryChild>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryChild {
    Transition(Transition),
    Extension(Extension),
}

/// An explicit initial pseudo-state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Initial {
    pub span: Span,
    pub attributes: Vec<Attribute>,
    pub transition: Option<Transition>,
    pub extensions: Vec<Extension>,
}

/// Children shared by state, parallel, and their SCXML regions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateChild {
    OnEntry(OnEntry),
    OnExit(OnExit),
    Initial(Initial),
    State(State),
    Parallel(Parallel),
    Final(Final),
    History(History),
    Transition(Transition),
    Invoke(Invoke),
    DataModel(DataModel),
    Extension(Extension),
}

/// A state entry handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnEntry {
    pub span: Span,
    pub attributes: Vec<Attribute>,
    pub content: Vec<ExecutableContent>,
}

/// A state exit handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnExit {
    pub span: Span,
    pub attributes: Vec<Attribute>,
    pub content: Vec<ExecutableContent>,
}

/// An SCXML transition. Event and target lists retain authored token order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub span: Span,
    pub event: Option<String>,
    pub events: Vec<String>,
    pub condition: Option<String>,
    pub targets: Vec<String>,
    pub transition_type: Option<TransitionType>,
    pub attributes: Vec<Attribute>,
    pub content: Vec<ExecutableContent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionType {
    Internal,
    External,
}

/// The SCXML data model container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataModel {
    pub span: Span,
    pub attributes: Vec<Attribute>,
    pub children: Vec<DataModelChild>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataModelChild {
    Data(Data),
    Extension(Extension),
}

/// A data model item. Expressions and locations are intentionally unparsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Data {
    pub span: Span,
    pub id: Option<String>,
    pub expression: Option<String>,
    pub source: Option<String>,
    pub text: String,
    pub content: Option<Content>,
    pub attributes: Vec<Attribute>,
}

/// An invoked service/activity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invoke {
    pub span: Span,
    pub id: Option<String>,
    pub invoke_type: Option<String>,
    pub type_expression: Option<String>,
    pub source: Option<String>,
    pub source_expression: Option<String>,
    pub id_location: Option<String>,
    pub namelist: Option<String>,
    pub autoforward: Option<String>,
    pub attributes: Vec<Attribute>,
    pub children: Vec<InvokeChild>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InvokeChild {
    Param(Param),
    Content(Content),
    Finalize(Finalize),
    Extension(Extension),
}

/// Standard message or done-event payload content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Content {
    pub span: Span,
    pub expression: Option<String>,
    pub attributes: Vec<Attribute>,
    pub parts: Vec<ContentPart>,
    /// Concatenated direct text content, convenient for scalar payloads.
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentPart {
    Text(String),
    Element(PayloadElement),
}

/// XML nested inside a value-bearing SCXML element. Payload elements are kept
/// separate from foreign extensions so capability analysis does not mistake
/// ordinary data for executable or extension syntax.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayloadElement {
    pub span: Span,
    pub name: String,
    pub local_name: String,
    pub namespace: Option<String>,
    pub attributes: Vec<Attribute>,
    pub content: Vec<PayloadPart>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PayloadPart {
    Text(String),
    Element(PayloadElement),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendChild {
    Param(Param),
    Content(Content),
    Extension(Extension),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DoneDataChild {
    Param(Param),
    Content(Content),
    Extension(Extension),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    pub span: Span,
    pub name: Option<String>,
    pub expression: Option<String>,
    pub location: Option<String>,
    pub attributes: Vec<Attribute>,
    pub extensions: Vec<Extension>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finalize {
    pub span: Span,
    pub attributes: Vec<Attribute>,
    pub content: Vec<ExecutableContent>,
}

/// Executable content is retained in authored order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutableContent {
    Raise(Raise),
    Assign(Assign),
    If(If),
    Foreach(Foreach),
    Log(Log),
    Send(Send),
    Cancel(Cancel),
    Script(Script),
    Extension(Extension),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Raise {
    pub span: Span,
    pub event: String,
    pub attributes: Vec<Attribute>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assign {
    pub span: Span,
    pub location: Option<String>,
    pub expression: Option<String>,
    pub source: Option<String>,
    pub content: Option<Content>,
    pub attributes: Vec<Attribute>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct If {
    pub span: Span,
    pub condition: Option<String>,
    pub attributes: Vec<Attribute>,
    pub children: Vec<IfChild>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IfChild {
    Executable(ExecutableContent),
    ElseIf(ElseIf),
    Else(Else),
    Extension(Extension),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElseIf {
    pub span: Span,
    pub condition: Option<String>,
    pub attributes: Vec<Attribute>,
    pub content: Vec<ExecutableContent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Else {
    pub span: Span,
    pub attributes: Vec<Attribute>,
    pub content: Vec<ExecutableContent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Foreach {
    pub span: Span,
    pub array: Option<String>,
    pub item: Option<String>,
    pub index: Option<String>,
    pub attributes: Vec<Attribute>,
    pub content: Vec<ExecutableContent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Log {
    pub span: Span,
    pub label: Option<String>,
    pub expression: Option<String>,
    pub attributes: Vec<Attribute>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Send {
    pub span: Span,
    pub event: Option<String>,
    pub event_expression: Option<String>,
    pub target: Option<String>,
    pub target_expression: Option<String>,
    pub send_type: Option<String>,
    pub type_expression: Option<String>,
    pub id: Option<String>,
    pub id_location: Option<String>,
    pub delay: Option<String>,
    pub delay_expression: Option<String>,
    pub namelist: Option<String>,
    pub attributes: Vec<Attribute>,
    pub children: Vec<SendChild>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cancel {
    pub span: Span,
    pub send_id: Option<String>,
    pub send_id_expression: Option<String>,
    pub attributes: Vec<Attribute>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Script {
    pub span: Span,
    pub source: Option<String>,
    pub text: String,
    pub attributes: Vec<Attribute>,
    pub extensions: Vec<Extension>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoneData {
    pub span: Span,
    pub attributes: Vec<Attribute>,
    pub children: Vec<DoneDataChild>,
}

/// An XML attribute with its resolved namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribute {
    pub name: String,
    pub local_name: String,
    pub namespace: Option<String>,
    pub value: String,
}

/// A foreign namespaced element retained without interpretation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extension {
    pub span: Span,
    pub name: String,
    pub local_name: String,
    pub namespace: Option<String>,
    pub attributes: Vec<Attribute>,
    pub content: Vec<ExtensionContent>,
    /// Concatenated text content, convenient for simple extension elements.
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtensionContent {
    Text(String),
    Element(Extension),
}

/// A byte range in the UTF-8 source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

/// A parsing failure with source name, byte span, and one-based line/column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub source_name: String,
    pub message: String,
    pub span: Span,
    pub line: usize,
    pub column: usize,
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}:{}:{}: {}",
            self.source_name, self.line, self.column, self.message
        )
    }
}

impl std::error::Error for Diagnostic {}

struct CapabilityAnalyzer<'a> {
    capabilities: &'a mut BTreeSet<Capability>,
}

impl<'a> CapabilityAnalyzer<'a> {
    fn new(capabilities: &'a mut BTreeSet<Capability>) -> Self {
        Self { capabilities }
    }

    fn add(&mut self, capability: Capability) {
        self.capabilities.insert(capability);
    }

    fn visit_scxml(&mut self, scxml: &Scxml) {
        let explicit_initial = !scxml.initial.is_empty();
        let has_state_children = scxml.children.iter().any(|child| {
            matches!(
                child,
                RootChild::State(_) | RootChild::Parallel(_) | RootChild::Final(_)
            )
        });
        if has_state_children || explicit_initial {
            self.add(Capability::CompoundState);
            if explicit_initial {
                self.add(Capability::ExplicitInitialTransition);
            } else {
                self.add(Capability::DefaultInitialTransition);
            }
        }

        for child in &scxml.children {
            self.visit_root_child(child);
        }
    }

    fn visit_root_child(&mut self, child: &RootChild) {
        match child {
            RootChild::DataModel(value) => self.visit_datamodel(value),
            RootChild::State(value) => self.visit_state(value),
            RootChild::Parallel(value) => self.visit_parallel(value),
            RootChild::Final(value) => self.visit_final(value),
            RootChild::Script(value) => {
                self.add(Capability::Script);
                self.visit_extensions(&value.extensions);
            }
            RootChild::Extension(value) => self.visit_extension(value),
        }
    }

    fn visit_state(&mut self, state: &State) {
        let explicit_initial = !state.initial.is_empty()
            || state
                .children
                .iter()
                .any(|child| matches!(child, StateChild::Initial(_)));
        let has_state_children = state.children.iter().any(|child| {
            matches!(
                child,
                StateChild::State(_) | StateChild::Parallel(_) | StateChild::Final(_)
            )
        });
        if has_state_children || explicit_initial {
            self.add(Capability::CompoundState);
            if explicit_initial {
                self.add(Capability::ExplicitInitialTransition);
            } else {
                self.add(Capability::DefaultInitialTransition);
            }
        }
        self.visit_state_children(&state.children);
    }

    fn visit_parallel(&mut self, parallel: &Parallel) {
        self.add(Capability::ParallelState);
        self.visit_parallel_children(&parallel.children);
    }

    fn visit_parallel_children(&mut self, children: &[ParallelChild]) {
        for child in children {
            match child {
                ParallelChild::OnEntry(value) => self.visit_content(&value.content),
                ParallelChild::OnExit(value) => self.visit_content(&value.content),
                ParallelChild::State(value) => self.visit_state(value),
                ParallelChild::Parallel(value) => self.visit_parallel(value),
                ParallelChild::History(value) => self.visit_history(value),
                ParallelChild::Transition(value) => self.visit_transition(value),
                ParallelChild::Invoke(value) => self.visit_invoke(value),
                ParallelChild::DataModel(value) => self.visit_datamodel(value),
                ParallelChild::Extension(value) => self.visit_extension(value),
            }
        }
    }

    fn visit_state_children(&mut self, children: &[StateChild]) {
        for child in children {
            match child {
                StateChild::OnEntry(value) => self.visit_content(&value.content),
                StateChild::OnExit(value) => self.visit_content(&value.content),
                StateChild::Initial(value) => self.visit_initial(value),
                StateChild::State(value) => self.visit_state(value),
                StateChild::Parallel(value) => self.visit_parallel(value),
                StateChild::Final(value) => self.visit_final(value),
                StateChild::History(value) => self.visit_history(value),
                StateChild::Transition(value) => self.visit_transition(value),
                StateChild::Invoke(value) => self.visit_invoke(value),
                StateChild::DataModel(value) => self.visit_datamodel(value),
                StateChild::Extension(value) => self.visit_extension(value),
            }
        }
    }

    fn visit_final(&mut self, final_state: &Final) {
        self.add(Capability::FinalState);
        for child in &final_state.children {
            match child {
                FinalChild::OnEntry(value) => self.visit_content(&value.content),
                FinalChild::OnExit(value) => self.visit_content(&value.content),
                FinalChild::DoneData(value) => self.visit_donedata(value),
                FinalChild::Extension(value) => self.visit_extension(value),
            }
        }
    }

    fn visit_history(&mut self, history: &History) {
        if history.history_type.as_deref() == Some("deep") {
            self.add(Capability::DeepHistory);
        } else {
            self.add(Capability::ShallowHistory);
        }
        for child in &history.children {
            match child {
                HistoryChild::Transition(value) => self.visit_transition(value),
                HistoryChild::Extension(value) => self.visit_extension(value),
            }
        }
    }

    fn visit_initial(&mut self, initial: &Initial) {
        self.add(Capability::ExplicitInitialTransition);
        if let Some(transition) = &initial.transition {
            self.visit_transition(transition);
        }
        self.visit_extensions(&initial.extensions);
    }

    fn visit_transition(&mut self, transition: &Transition) {
        let event_tokens: Vec<&str> = if transition.events.is_empty() {
            transition
                .event
                .as_deref()
                .into_iter()
                .flat_map(str::split_whitespace)
                .collect()
        } else {
            transition.events.iter().map(String::as_str).collect()
        };
        if event_tokens.is_empty() {
            self.add(Capability::EventlessTransition);
        }
        if event_tokens.len() > 1 {
            self.add(Capability::MultiEventTransition);
        }
        for event in event_tokens {
            if event == "*" {
                self.add(Capability::WildcardEventDescriptor);
            } else if event.ends_with(".*") {
                self.add(Capability::PrefixEventDescriptor);
            }
        }
        if transition.targets.is_empty() {
            self.add(Capability::TargetlessTransition);
        } else if transition.targets.len() > 1 {
            self.add(Capability::MultiTargetTransition);
        }
        if transition.condition.is_some() {
            self.add(Capability::Conditions);
        }
        if transition.transition_type == Some(TransitionType::Internal) {
            self.add(Capability::InternalTransition);
        }
        self.visit_content(&transition.content);
    }

    fn visit_datamodel(&mut self, datamodel: &DataModel) {
        self.add(Capability::DataModel);
        for child in &datamodel.children {
            match child {
                DataModelChild::Data(value) => self.add_data(value),
                DataModelChild::Extension(value) => self.visit_extension(value),
            }
        }
    }

    fn add_data(&mut self, data: &Data) {
        self.add(Capability::Data);
        if let Some(content) = &data.content {
            self.visit_payload_content(content);
        }
    }

    fn visit_invoke(&mut self, invoke: &Invoke) {
        self.add(Capability::Invoke);
        for child in &invoke.children {
            match child {
                InvokeChild::Param(value) => {
                    self.add(Capability::Param);
                    self.visit_extensions(&value.extensions);
                }
                InvokeChild::Content(value) => self.visit_content_node(value),
                InvokeChild::Finalize(value) => {
                    self.add(Capability::Finalize);
                    self.visit_content(&value.content);
                }
                InvokeChild::Extension(value) => self.visit_extension(value),
            }
        }
    }

    fn visit_donedata(&mut self, donedata: &DoneData) {
        for child in &donedata.children {
            match child {
                DoneDataChild::Param(value) => {
                    self.add(Capability::Param);
                    self.visit_extensions(&value.extensions);
                }
                DoneDataChild::Content(value) => self.visit_content_node(value),
                DoneDataChild::Extension(value) => self.visit_extension(value),
            }
        }
    }

    fn visit_content_node(&mut self, content: &Content) {
        self.add(Capability::Content);
        self.visit_payload_content(content);
    }

    fn visit_payload_content(&mut self, content: &Content) {
        self.add(Capability::PayloadContent);
        for part in &content.parts {
            let ContentPart::Element(value) = part else {
                continue;
            };
            self.visit_payload_element(value);
        }
    }

    fn visit_payload_element(&mut self, element: &PayloadElement) {
        for part in &element.content {
            if let PayloadPart::Element(value) = part {
                self.visit_payload_element(value);
            }
        }
    }

    fn visit_content(&mut self, content: &[ExecutableContent]) {
        for executable in content {
            self.visit_executable(executable);
        }
    }

    fn visit_executable(&mut self, executable: &ExecutableContent) {
        match executable {
            ExecutableContent::Raise(_) => self.add(Capability::Raise),
            ExecutableContent::Assign(value) => {
                self.add(Capability::Assign);
                if let Some(content) = &value.content {
                    self.visit_payload_content(content);
                }
            }
            ExecutableContent::If(value) => {
                self.add(Capability::If);
                if value.condition.is_some() {
                    self.add(Capability::Conditions);
                }
                for child in &value.children {
                    match child {
                        IfChild::Executable(value) => self.visit_executable(value),
                        IfChild::ElseIf(value) => {
                            if value.condition.is_some() {
                                self.add(Capability::Conditions);
                            }
                            self.visit_content(&value.content);
                        }
                        IfChild::Else(value) => self.visit_content(&value.content),
                        IfChild::Extension(value) => self.visit_extension(value),
                    }
                }
            }
            ExecutableContent::Foreach(value) => {
                self.add(Capability::Foreach);
                self.visit_content(&value.content);
            }
            ExecutableContent::Log(_) => self.add(Capability::Log),
            ExecutableContent::Send(value) => {
                self.add(Capability::Send);
                for child in &value.children {
                    match child {
                        SendChild::Param(value) => {
                            self.add(Capability::Param);
                            self.visit_extensions(&value.extensions);
                        }
                        SendChild::Content(value) => self.visit_content_node(value),
                        SendChild::Extension(value) => self.visit_extension(value),
                    }
                }
            }
            ExecutableContent::Cancel(_) => self.add(Capability::Cancel),
            ExecutableContent::Script(value) => {
                self.add(Capability::Script);
                self.visit_extensions(&value.extensions);
            }
            ExecutableContent::Extension(value) => self.visit_extension(value),
        }
    }

    fn visit_extensions(&mut self, extensions: &[Extension]) {
        for extension in extensions {
            self.visit_extension(extension);
        }
    }

    fn visit_extension(&mut self, extension: &Extension) {
        self.add(Capability::ForeignExtension);
        for child in &extension.content {
            if let ExtensionContent::Element(child) = child {
                self.visit_extension(child);
            }
        }
    }
}

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
                Ok(Event::Text(text)) if text.as_ref().iter().all(u8::is_ascii_whitespace) => {}
                Ok(Event::CData(text)) if text.as_ref().iter().all(u8::is_ascii_whitespace) => {}
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
                Ok(Event::Text(text)) if text.as_ref().iter().all(u8::is_ascii_whitespace) => {}
                Ok(Event::CData(text)) if text.as_ref().iter().all(u8::is_ascii_whitespace) => {}
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

        match reference.as_ref() {
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
            "invoke" => StateChild::Invoke(decode_invoke(child, source_name, source)?),
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
            "invoke" => ParallelChild::Invoke(decode_invoke(child, source_name, source)?),
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
    Ok(Transition {
        span: element.span,
        events: event
            .as_deref()
            .map(|value| value.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default(),
        event,
        condition: attribute(element, "cond").map(str::to_owned),
        targets: attribute(element, "target")
            .map(|target| target.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default(),
        transition_type,
        attributes: element.attributes.clone(),
        content: decode_executable_children(element, source_name, source)?,
    })
}

fn decode_invoke(element: &Element, source_name: &str, source: &str) -> Result<Invoke, Diagnostic> {
    let mut children = Vec::new();
    for child in &element.children {
        if !is_scxml(child) {
            children.push(InvokeChild::Extension(extension_from_element(child)));
            continue;
        }
        match child.local_name.as_str() {
            "param" => children.push(InvokeChild::Param(decode_param(
                child,
                source_name,
                source,
            )?)),
            "content" => children.push(InvokeChild::Content(decode_content(child))),
            "finalize" => children.push(InvokeChild::Finalize(decode_finalize(
                child,
                source_name,
                source,
            )?)),
            _ => return Err(unsupported_scxml(source_name, source, child, "invoke")),
        }
    }
    Ok(Invoke {
        span: element.span,
        id: attribute(element, "id").map(str::to_owned),
        invoke_type: attribute(element, "type").map(str::to_owned),
        type_expression: attribute(element, "typeexpr").map(str::to_owned),
        source: attribute(element, "src").map(str::to_owned),
        source_expression: attribute(element, "srcexpr").map(str::to_owned),
        id_location: attribute(element, "idlocation").map(str::to_owned),
        namelist: attribute(element, "namelist").map(str::to_owned),
        autoforward: attribute(element, "autoforward").map(str::to_owned),
        attributes: element.attributes.clone(),
        children,
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

fn decode_finalize(
    element: &Element,
    source_name: &str,
    source: &str,
) -> Result<Finalize, Diagnostic> {
    Ok(Finalize {
        span: element.span,
        attributes: element.attributes.clone(),
        content: decode_executable_children(element, source_name, source)?,
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
                | "invoke"
                | "param"
                | "finalize"
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
    fn decodes_parallel_history_initial_final_and_invoke_nodes() {
        let source = format!(
            r#"<scxml xmlns="{NS}"><parallel id="regions"><state id="region"><history id="remember" type="deep"><transition target="region"/></history><initial><transition target="region"/></initial><invoke id="service" type="example.service"><param name="value" expr="ctx.value"/><finalize><assign location="ctx.done" expr="true"/></finalize></invoke><final id="finished"><onexit><log expr="done"/></onexit></final></state></parallel></scxml>"#
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
        let invoke = match &state.children[2] {
            StateChild::Invoke(value) => value,
            _ => panic!("invoke"),
        };
        assert!(matches!(invoke.children[0], InvokeChild::Param(_)));
        assert!(matches!(invoke.children[1], InvokeChild::Finalize(_)));
        assert!(matches!(state.children[3], StateChild::Final(_)));
    }

    #[test]
    fn parses_optional_ids_payload_nodes_local_datamodel_and_sequence_conditionals() {
        let source = format!(
            r#"<scxml xmlns="{NS}" xmlns:x="urn:payload"><script>root script</script><state><datamodel><data expr="raw"/></datamodel><onentry><if cond="first"><raise event="first"/><elseif cond="second"/><log expr="second"/><else/><script>fallback</script></if><send event="message"><param name="one"/><content>body<x:value/></content></send></onentry><invoke><param name="argument"/><content><x:request/></content></invoke><final><donedata><param name="result"/><content>done</content></donedata></final></state></scxml>"#
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
        let invoke = match &state.children[2] {
            StateChild::Invoke(value) => value,
            _ => panic!("invoke"),
        };
        assert!(matches!(invoke.children[0], InvokeChild::Param(_)));
        assert!(matches!(invoke.children[1], InvokeChild::Content(_)));
        let final_state = match &state.children[3] {
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
    fn preserves_reviewed_w3c_payload_and_standard_attribute_patterns() {
        let fixture_557 = parse(
            "557/test557.txml",
            include_str!("../../tests/w3c/vendor/557/test557.txml"),
        )
        .expect("fixture 557");
        let data = match &fixture_557.root.children[0] {
            RootChild::DataModel(model) => match &model.children[0] {
                DataModelChild::Data(data) => data,
                _ => panic!("fixture 557 data"),
            },
            _ => panic!("fixture 557 datamodel"),
        };
        let payload = data.content.as_ref().expect("fixture 557 data payload");
        let books = payload.parts.iter().find_map(|part| match part {
            ContentPart::Element(value) => Some(value),
            ContentPart::Text(_) => None,
        });
        assert_eq!(books.map(|value| value.local_name.as_str()), Some("books"));
        assert!(matches!(
            books.and_then(|value| value.content.first()),
            Some(PayloadPart::Text(_))
        ));

        let fixture_530 = parse(
            "530/test530.txml",
            include_str!("../../tests/w3c/vendor/530/test530.txml"),
        )
        .expect("fixture 530");
        let state = match &fixture_530.root.children[1] {
            RootChild::State(state) => state,
            _ => panic!("fixture 530 state"),
        };
        let entry = match &state.children[0] {
            StateChild::OnEntry(entry) => entry,
            _ => panic!("fixture 530 onentry"),
        };
        let assign = match &entry.content[0] {
            ExecutableContent::Assign(assign) => assign,
            _ => panic!("fixture 530 assign"),
        };
        assert!(assign.content.is_some());
        let invoke = match &state.children[1] {
            StateChild::Invoke(invoke) => invoke,
            _ => panic!("fixture 530 invoke"),
        };
        assert!(matches!(invoke.children[0], InvokeChild::Content(_)));

        let fixture_576 = parse(
            "576/test576.txml",
            include_str!("../../tests/w3c/vendor/576/test576.txml"),
        )
        .expect("fixture 576");
        assert_eq!(fixture_576.root.initial, ["s11p112", "s11p122"]);

        let fixture_364 = parse(
            "364/test364.txml",
            include_str!("../../tests/w3c/vendor/364/test364.txml"),
        )
        .expect("fixture 364");
        let state_s1 = match &fixture_364.root.children[0] {
            RootChild::State(state) => state,
            _ => panic!("fixture 364 state s1"),
        };
        assert_eq!(state_s1.initial, ["s11p112", "s11p122"]);
        let state_s2 = match &fixture_364.root.children[1] {
            RootChild::State(state) => state,
            _ => panic!("fixture 364 state s2"),
        };
        let initial = match &state_s2.children[0] {
            StateChild::Initial(initial) => initial,
            _ => panic!("fixture 364 explicit initial"),
        };
        assert_eq!(
            initial.transition.as_ref().unwrap().targets,
            ["s21p112", "s21p122"]
        );

        let fixture_200 = parse(
            "200/test200.txml",
            include_str!("../../tests/w3c/vendor/200/test200.txml"),
        )
        .expect("fixture 200");
        let state = match &fixture_200.root.children[0] {
            RootChild::State(state) => state,
            _ => panic!("fixture 200 state"),
        };
        let entry = match &state.children[0] {
            StateChild::OnEntry(entry) => entry,
            _ => panic!("fixture 200 onentry"),
        };
        let send = match &entry.content[0] {
            ExecutableContent::Send(send) => send,
            _ => panic!("fixture 200 send"),
        };
        assert_eq!(
            send.send_type.as_deref(),
            Some("http://www.w3.org/TR/scxml/#SCXMLEventProcessor")
        );

        let fixture_223 = parse(
            "223/test223.txml",
            include_str!("../../tests/w3c/vendor/223/test223.txml"),
        )
        .expect("fixture 223");
        let state = match &fixture_223.root.children[1] {
            RootChild::State(state) => state,
            _ => panic!("fixture 223 state"),
        };
        let invoke = match &state.children[1] {
            StateChild::Invoke(invoke) => invoke,
            _ => panic!("fixture 223 invoke"),
        };
        assert_eq!(invoke.id_location, None);
        assert!(
            invoke
                .attributes
                .iter()
                .any(|attribute| attribute.name == "conf:idlocation")
        );
        let content = match &invoke.children[0] {
            InvokeChild::Content(content) => content,
            _ => panic!("fixture 223 content"),
        };
        assert!(
            content
                .parts
                .iter()
                .any(|part| matches!(part, ContentPart::Element(_)))
        );

        let fixture_245 = parse(
            "245/test245.txml",
            include_str!("../../tests/w3c/vendor/245/test245.txml"),
        )
        .expect("fixture 245");
        let state = match &fixture_245.root.children[1] {
            RootChild::State(state) => state,
            _ => panic!("fixture 245 state"),
        };
        let invoke = match &state.children[1] {
            StateChild::Invoke(invoke) => invoke,
            _ => panic!("fixture 245 invoke"),
        };
        assert_eq!(invoke.namelist, None);
        assert!(
            invoke
                .attributes
                .iter()
                .any(|attribute| attribute.name == "conf:namelist")
        );

        let fixture_527 = parse(
            "527/test527.txml",
            include_str!("../../tests/w3c/vendor/527/test527.txml"),
        )
        .expect("fixture 527");
        let state = match &fixture_527.root.children[0] {
            RootChild::State(state) => state,
            _ => panic!("fixture 527 state"),
        };
        let nested = state
            .children
            .iter()
            .find_map(|child| match child {
                StateChild::State(state) => Some(state),
                _ => None,
            })
            .expect("fixture 527 nested state");
        assert_eq!(nested.id.as_deref(), Some("s01"));
        let final_state = state
            .children
            .iter()
            .find_map(|child| match child {
                StateChild::Final(final_state) => Some(final_state),
                _ => None,
            })
            .expect("fixture 527 final");
        let done_data = match &final_state.children[0] {
            FinalChild::DoneData(done_data) => done_data,
            _ => panic!("fixture 527 donedata"),
        };
        let content = match &done_data.children[0] {
            DoneDataChild::Content(content) => content,
            _ => panic!("fixture 527 content"),
        };
        assert_eq!(content.expression, None);

        let payload_document = parse(
            "payload-capability.scxml",
            &format!(
                r#"<scxml xmlns="{NS}" xmlns:x="urn:payload"><state><onentry><assign location="value">before<x:value/>after</assign><send event="payload"><content><x:message/></content></send></onentry></state></scxml>"#
            ),
        )
        .expect("payload capability document");
        let capabilities = payload_document.capabilities();
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
                  <invoke id="service"><finalize><assign location="done" expr="true"/></finalize><x:invoke/></invoke>
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
            Capability::Invoke,
            Capability::Finalize,
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
        parse("payload-text.scxml", &payload).expect("payload text");
    }
}
