//! Namespace-aware SCXML parsing and compilation.
//!
//! Parsing preserves authored syntax without resolving state ids or evaluating
//! expressions. [`compile`] separately lowers the supported subset into the
//! production runtime representation and rejects unsupported semantics.

mod analysis;
mod ast;
mod compile;
mod parse;

pub use ast::{
    Assign, Attribute, Cancel, Capability, Content, ContentPart, Data, DataModel, DataModelChild,
    Diagnostic, Document, DoneData, DoneDataChild, Else, ElseIf, ExecutableContent, Extension,
    ExtensionContent, Final, FinalChild, Finalize, Foreach, History, HistoryChild, If, IfChild,
    Initial, Invoke, InvokeChild, Log, OnEntry, OnExit, Parallel, ParallelChild, Param,
    PayloadElement, PayloadPart, Raise, RootChild, Script, Scxml, Send, SendChild, Span, State,
    StateChild, Transition, TransitionType,
};
pub use compile::{CompileError, compile};
pub use parse::{SCXML_NAMESPACE, parse};

/// Namespace URI for Bevy-specific executable content.
pub const BEVY_STATE_TREE_NAMESPACE: &str = "urn:bevy-state-tree";
