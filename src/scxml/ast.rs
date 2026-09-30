use std::time::Duration;

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
/// ordering are stable so a returned [`std::collections::BTreeSet`] can be compared or serialized
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
    /// A state-scoped after transition is present.
    AfterTransition,
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
    /// A Bevy state-tree typed effect is present.
    TypedEffect,
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
    pub after: Option<Duration>,
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

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{}:{}:{}: {}",
            self.source_name, self.line, self.column, self.message
        )
    }
}

impl std::error::Error for Diagnostic {}
