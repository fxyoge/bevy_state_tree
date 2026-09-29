use std::collections::{HashMap, HashSet};

use thiserror::Error;

use super::{
    Attribute, Content, ContentPart, DataModelChild, Document, ExecutableContent, Final,
    FinalChild, Initial, OnEntry, OnExit, RootChild, Span, State, StateChild, Transition,
    TransitionType,
};
use crate::{
    BuildError, StateId, StateTreeBuilder, StateTreeDefinition,
    definition::{DataId, DraftAction},
    luau::compile_expression,
};

/// Compiles the supported SCXML subset into the production state-tree runtime format.
pub fn compile(document: &Document) -> Result<StateTreeDefinition, CompileError> {
    Compiler::new(document).compile()
}

#[derive(Debug, Error)]
pub enum CompileError {
    #[error("{source_name}@{span_start}: unsupported SCXML feature: {feature}")]
    Unsupported {
        source_name: String,
        span_start: usize,
        feature: &'static str,
    },
    #[error("{source_name}@{span_start}: unknown state target `{target}`")]
    UnknownTarget {
        source_name: String,
        span_start: usize,
        target: String,
    },
    #[error("{source_name}@{span_start}: an initial transition must have exactly one target")]
    InvalidInitial {
        source_name: String,
        span_start: usize,
    },
    #[error("{source_name}@{span_start}: invalid Luau expression: {message}")]
    InvalidLuau {
        source_name: String,
        span_start: usize,
        message: String,
    },
    #[error("{source_name}@{span_start}: invalid Luau datamodel: {message}")]
    InvalidDataModel {
        source_name: String,
        span_start: usize,
        message: String,
    },
    #[error(transparent)]
    Build(#[from] BuildError),
}

#[derive(Clone, Copy)]
enum Node<'a> {
    State(&'a State),
    Final,
}

struct Record<'a> {
    node: Node<'a>,
    runtime: StateId,
    parent: StateId,
}

struct Compiler<'a> {
    document: &'a Document,
    builder: StateTreeBuilder,
    records: Vec<Record<'a>>,
    ids: HashMap<&'a str, StateId>,
    used_names: HashSet<String>,
    root_name: String,
    next_anonymous: usize,
    data_ids: HashMap<String, DataId>,
}

impl<'a> Compiler<'a> {
    fn new(document: &'a Document) -> Self {
        let mut used_names = HashSet::new();
        collect_ids(&document.root.children, &mut used_names);
        let root_name = unique_name("#document", &mut used_names);
        Self {
            document,
            builder: StateTreeBuilder::new(
                document
                    .root
                    .name
                    .as_deref()
                    .unwrap_or(&document.source_name),
            ),
            records: Vec::new(),
            ids: HashMap::new(),
            used_names,
            root_name,
            next_anonymous: 0,
            data_ids: HashMap::new(),
        }
    }

    fn compile(mut self) -> Result<StateTreeDefinition, CompileError> {
        self.reject_root_features()?;
        self.compile_datamodel()?;
        let root = self.builder.compound_root(self.root_name.clone())?;

        for child in &self.document.root.children {
            match child {
                RootChild::State(state) => self.add_state(state, root)?,
                RootChild::Final(state) => self.add_final(state, root)?,
                _ => {}
            }
        }
        self.set_initial(
            root,
            self.document.root.span,
            &self.document.root.initial,
            None,
        )?;

        for index in 0..self.records.len() {
            self.compile_record(index)?;
        }
        Ok(self.builder.build()?)
    }

    fn reject_root_features(&self) -> Result<(), CompileError> {
        if self
            .document
            .root
            .datamodel
            .as_deref()
            .is_some_and(|value| !matches!(value, "null" | "luau"))
        {
            return self.unsupported(self.document.root.span, "datamodel selection");
        }
        if self
            .document
            .root
            .binding
            .as_deref()
            .is_some_and(|binding| binding != "early")
        {
            return self.unsupported(self.document.root.span, "datamodel binding");
        }
        if has_foreign_attributes(&self.document.root.attributes) {
            return self.unsupported(self.document.root.span, "foreign root attributes");
        }
        for child in &self.document.root.children {
            let (span, feature) = match child {
                RootChild::State(_) | RootChild::Final(_) | RootChild::DataModel(_) => continue,
                RootChild::Parallel(value) => (value.span, "parallel states"),
                RootChild::Script(value) => (value.span, "scripts"),
                RootChild::Extension(value) => (value.span, "foreign extensions"),
            };
            return self.unsupported(span, feature);
        }
        Ok(())
    }

    fn compile_datamodel(&mut self) -> Result<(), CompileError> {
        let models: Vec<_> = self
            .document
            .root
            .children
            .iter()
            .filter_map(|child| match child {
                RootChild::DataModel(model) => Some(model),
                _ => None,
            })
            .collect();
        if models.len() > 1 {
            return self.invalid_datamodel(
                self.document.root.span,
                "the root may contain only one datamodel",
            );
        }
        let selected = self.document.root.datamodel.as_deref();
        if selected == Some("luau") || !models.is_empty() {
            if selected != Some("luau") {
                return self.invalid_datamodel(
                    self.document.root.span,
                    "a root datamodel requires datamodel=\"luau\"",
                );
            }
            let mut names = Vec::<Box<str>>::new();
            let mut initializers = Vec::new();
            if let Some(model) = models.first() {
                if has_foreign_attributes(&model.attributes) {
                    return self.unsupported(model.span, "foreign datamodel attributes");
                }
                for child in &model.children {
                    let DataModelChild::Data(data) = child else {
                        let DataModelChild::Extension(extension) = child else {
                            unreachable!()
                        };
                        return self.unsupported(extension.span, "foreign datamodel extensions");
                    };
                    if has_foreign_attributes(&data.attributes)
                        || data.source.is_some()
                        || has_substantive_content(data.content.as_ref())
                        || !data.text.trim().is_empty()
                    {
                        return self.unsupported(data.span, "non-expression Luau data");
                    }
                    let Some(id) = data.id.as_deref() else {
                        return self.invalid_datamodel(data.span, "data requires an id");
                    };
                    if !is_luau_identifier(id) {
                        return self.invalid_datamodel(
                            data.span,
                            format!("`{id}` is not a Luau identifier"),
                        );
                    }
                    if is_scxml_system_name(id) {
                        return self.invalid_datamodel(
                            data.span,
                            format!("`{id}` is reserved by the SCXML runtime"),
                        );
                    }
                    if names.iter().any(|name| name.as_ref() == id) {
                        return self
                            .invalid_datamodel(data.span, format!("data id `{id}` is duplicated"));
                    }
                    let initializer = data
                        .expression
                        .as_deref()
                        .map(|expression| self.compile_luau(expression, false, data.span))
                        .transpose()?;
                    names.push(Box::<str>::from(id));
                    initializers.push(initializer);
                }
            }
            let ids = self.builder.set_luau_datamodel(names, initializers)?;
            if let Some(model) = models.first() {
                for (child, id) in model.children.iter().zip(ids) {
                    if let DataModelChild::Data(data) = child {
                        self.data_ids.insert(data.id.clone().unwrap(), id);
                    }
                }
            }
        }
        Ok(())
    }

    fn add_state(&mut self, state: &'a State, parent: StateId) -> Result<(), CompileError> {
        if has_foreign_attributes(&state.attributes) {
            return self.unsupported(state.span, "foreign state attributes");
        }
        let compound = state.children.iter().any(|child| {
            matches!(
                child,
                StateChild::State(_) | StateChild::Final(_) | StateChild::Initial(_)
            )
        });
        let name = self.node_name(state.id.as_deref());
        let runtime = if compound {
            self.builder.compound(name, parent)?
        } else {
            self.builder.atomic(name, parent)?
        };
        if let Some(id) = state.id.as_deref() {
            self.ids.insert(id, runtime);
        }
        let mut entry_actions = Vec::new();
        let mut exit_actions = Vec::new();
        for child in &state.children {
            match child {
                StateChild::OnEntry(value) => entry_actions.extend(self.entry_actions(value)?),
                StateChild::OnExit(value) => exit_actions.extend(self.exit_actions(value)?),
                _ => {}
            }
        }
        self.builder.entry_actions(runtime, entry_actions)?;
        self.builder.exit_actions(runtime, exit_actions)?;
        self.records.push(Record {
            node: Node::State(state),
            runtime,
            parent,
        });
        for child in &state.children {
            match child {
                StateChild::State(child) => self.add_state(child, runtime)?,
                StateChild::Final(child) => self.add_final(child, runtime)?,
                StateChild::Parallel(value) => {
                    return self.unsupported(value.span, "parallel states");
                }
                StateChild::History(value) => {
                    return self.unsupported(value.span, "history states");
                }
                StateChild::Invoke(value) => {
                    return self.unsupported(value.span, "invocation");
                }
                StateChild::DataModel(value) => {
                    return self.unsupported(value.span, "datamodels");
                }
                StateChild::Extension(value) => {
                    return self.unsupported(value.span, "foreign extensions");
                }
                StateChild::OnEntry(_) | StateChild::OnExit(_) => {}
                StateChild::Initial(_) | StateChild::Transition(_) => {}
            }
        }
        Ok(())
    }

    fn add_final(&mut self, state: &'a Final, parent: StateId) -> Result<(), CompileError> {
        if has_foreign_attributes(&state.attributes) {
            return self.unsupported(state.span, "foreign final-state attributes");
        }
        let mut entry_actions = Vec::new();
        let mut exit_actions = Vec::new();
        for child in &state.children {
            match child {
                FinalChild::OnEntry(value) => entry_actions.extend(self.entry_actions(value)?),
                FinalChild::OnExit(value) => exit_actions.extend(self.exit_actions(value)?),
                FinalChild::DoneData(value) => {
                    return self.unsupported(value.span, "done data");
                }
                FinalChild::Extension(value) => {
                    return self.unsupported(value.span, "foreign extensions");
                }
            }
        }
        let name = self.node_name(state.id.as_deref());
        let runtime = self.builder.final_state(name, parent)?;
        self.builder.entry_actions(runtime, entry_actions)?;
        self.builder.exit_actions(runtime, exit_actions)?;
        if let Some(id) = state.id.as_deref() {
            self.ids.insert(id, runtime);
        }
        self.records.push(Record {
            node: Node::Final,
            runtime,
            parent,
        });
        Ok(())
    }

    fn compile_record(&mut self, index: usize) -> Result<(), CompileError> {
        let node = self.records[index].node;
        let runtime = self.records[index].runtime;
        let Node::State(state) = node else {
            return Ok(());
        };
        let explicit = state.children.iter().find_map(|child| match child {
            StateChild::Initial(initial) => Some(initial),
            _ => None,
        });
        if state
            .children
            .iter()
            .filter(|child| matches!(child, StateChild::Initial(_)))
            .count()
            > 1
        {
            return self.unsupported(state.span, "multiple initial declarations");
        }
        self.set_initial(runtime, state.span, &state.initial, explicit)?;
        for child in &state.children {
            if let StateChild::Transition(transition) = child {
                self.compile_transition(runtime, transition)?;
            }
        }
        Ok(())
    }

    fn set_initial(
        &mut self,
        state: StateId,
        span: Span,
        attribute: &[String],
        explicit: Option<&Initial>,
    ) -> Result<(), CompileError> {
        let child = if let Some(initial) = explicit {
            if has_foreign_attributes(&initial.attributes) || !initial.extensions.is_empty() {
                return self.unsupported(initial.span, "extended initial declarations");
            }
            if !attribute.is_empty() {
                return self.unsupported(initial.span, "multiple initial declarations");
            }
            let Some(transition) = initial.transition.as_ref() else {
                return self.invalid_initial(initial.span);
            };
            let actions = self.check_initial_transition(transition)?;
            self.builder.initial_actions(state, actions)?;
            self.target(
                transition.targets.first().expect("checked target"),
                transition.span,
            )?
        } else if !attribute.is_empty() {
            if attribute.len() != 1 {
                return self.invalid_initial(span);
            }
            self.target(&attribute[0], span)?
        } else {
            let Some(child) = self
                .records
                .iter()
                .find(|record| record.parent == state)
                .map(|record| record.runtime)
            else {
                return Ok(());
            };
            child
        };
        self.builder.initial(state, child)?;
        Ok(())
    }

    fn check_initial_transition(
        &mut self,
        transition: &Transition,
    ) -> Result<Vec<DraftAction>, CompileError> {
        if transition.targets.len() != 1 {
            return self.invalid_initial(transition.span);
        }
        if transition.event.is_some()
            || transition.condition.is_some()
            || transition.transition_type.is_some()
            || has_foreign_attributes(&transition.attributes)
        {
            return self.unsupported(transition.span, "executable initial transitions");
        }
        self.actions(&transition.content, transition.span)
    }

    fn compile_transition(
        &mut self,
        source: StateId,
        transition: &Transition,
    ) -> Result<(), CompileError> {
        if transition.transition_type == Some(TransitionType::Internal) {
            return self.unsupported(transition.span, "internal transitions");
        }
        let condition = transition
            .condition
            .as_deref()
            .map(|condition| {
                if self.document.root.datamodel.as_deref() != Some("luau") {
                    return self.invalid_datamodel(
                        transition.span,
                        "transition conditions require datamodel=\"luau\"",
                    );
                }
                let expression = self.compile_luau(condition, true, transition.span)?;
                self.builder
                    .luau_expression(expression)
                    .map_err(CompileError::from)
            })
            .transpose()?;
        let actions = self.actions(&transition.content, transition.span)?;
        if has_foreign_attributes(&transition.attributes) {
            return self.unsupported(transition.span, "foreign transition attributes");
        }
        if transition.targets.len() != 1 {
            return self.unsupported(transition.span, "targetless or multi-target transitions");
        }
        let target = self.target(&transition.targets[0], transition.span)?;
        if transition.events.is_empty() {
            self.builder
                .eventless_with_actions(source, condition, target, actions)?;
        } else {
            for event in &transition.events {
                self.builder.event_descriptor_with_actions(
                    source,
                    event.clone(),
                    condition,
                    target,
                    actions.clone(),
                )?;
            }
        }
        Ok(())
    }

    fn entry_actions(&mut self, entry: &OnEntry) -> Result<Vec<DraftAction>, CompileError> {
        if has_foreign_attributes(&entry.attributes) {
            return self.unsupported(entry.span, "foreign onentry attributes");
        }
        self.actions(&entry.content, entry.span)
    }

    fn exit_actions(&mut self, exit: &OnExit) -> Result<Vec<DraftAction>, CompileError> {
        if has_foreign_attributes(&exit.attributes) {
            return self.unsupported(exit.span, "foreign onexit attributes");
        }
        self.actions(&exit.content, exit.span)
    }

    fn actions(
        &mut self,
        content: &[ExecutableContent],
        span: Span,
    ) -> Result<Vec<DraftAction>, CompileError> {
        content
            .iter()
            .map(|executable| match executable {
                ExecutableContent::Raise(raise) if !has_foreign_attributes(&raise.attributes) => {
                    Ok(DraftAction::Raise(Box::from(raise.event.as_str())))
                }
                ExecutableContent::Raise(raise) => {
                    self.unsupported(raise.span, "foreign raise attributes")
                }
                ExecutableContent::Assign(assign)
                    if !has_foreign_attributes(&assign.attributes)
                        && assign.source.is_none()
                        && !has_substantive_content(assign.content.as_ref()) =>
                {
                    let Some(location) = assign.location.as_deref() else {
                        return self.invalid_datamodel(assign.span, "assign requires a location");
                    };
                    let Some(data) = self.data_ids.get(location).copied() else {
                        return self.invalid_datamodel(
                            assign.span,
                            format!("assignment location `{location}` is not declared"),
                        );
                    };
                    let Some(source) = assign.expression.as_deref() else {
                        return self
                            .invalid_datamodel(assign.span, "assign requires an expression");
                    };
                    let expression = self.compile_luau(source, false, assign.span)?;
                    let expression = self.builder.luau_expression(expression)?;
                    Ok(DraftAction::Assign { data, expression })
                }
                ExecutableContent::Assign(assign) => {
                    self.unsupported(assign.span, "non-expression Luau assignment")
                }
                _ => self.unsupported(span, "executable content other than raise"),
            })
            .collect()
    }

    fn compile_luau(
        &self,
        source: &str,
        boolean: bool,
        span: Span,
    ) -> Result<crate::luau::CompiledLuauExpression, CompileError> {
        compile_expression(source, boolean, span.start).map_err(|error| CompileError::InvalidLuau {
            source_name: self.document.source_name.clone(),
            span_start: span.start,
            message: error.to_string(),
        })
    }

    fn target(&self, target: &str, span: Span) -> Result<StateId, CompileError> {
        self.ids
            .get(target)
            .copied()
            .ok_or_else(|| CompileError::UnknownTarget {
                source_name: self.document.source_name.clone(),
                span_start: span.start,
                target: target.to_owned(),
            })
    }

    fn node_name(&mut self, id: Option<&str>) -> String {
        if let Some(id) = id {
            return id.to_owned();
        }
        loop {
            let candidate = format!("#anonymous{}", self.next_anonymous);
            self.next_anonymous += 1;
            if self.used_names.insert(candidate.clone()) {
                return candidate;
            }
        }
    }

    fn unsupported<T>(&self, span: Span, feature: &'static str) -> Result<T, CompileError> {
        Err(CompileError::Unsupported {
            source_name: self.document.source_name.clone(),
            span_start: span.start,
            feature,
        })
    }

    fn invalid_initial<T>(&self, span: Span) -> Result<T, CompileError> {
        Err(CompileError::InvalidInitial {
            source_name: self.document.source_name.clone(),
            span_start: span.start,
        })
    }

    fn invalid_datamodel<T>(
        &self,
        span: Span,
        message: impl Into<String>,
    ) -> Result<T, CompileError> {
        Err(CompileError::InvalidDataModel {
            source_name: self.document.source_name.clone(),
            span_start: span.start,
            message: message.into(),
        })
    }
}

fn is_luau_identifier(value: &str) -> bool {
    const RESERVED: &[&str] = &[
        "and", "break", "continue", "do", "else", "elseif", "end", "export", "false", "for",
        "function", "if", "in", "local", "nil", "not", "or", "repeat", "return", "then", "true",
        "type", "until", "while",
    ];
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte == b'_' || byte.is_ascii_alphabetic())
        && bytes.all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
        && !RESERVED.contains(&value)
}

fn is_scxml_system_name(value: &str) -> bool {
    matches!(
        value,
        "_event" | "_sessionid" | "_name" | "_ioprocessors" | "_x" | "In"
    )
}

fn collect_ids(children: &[RootChild], ids: &mut HashSet<String>) {
    fn collect_state(current: &State, ids: &mut HashSet<String>) {
        if let Some(id) = &current.id {
            ids.insert(id.clone());
        }
        for child in &current.children {
            match child {
                StateChild::State(child) => collect_state(child, ids),
                StateChild::Final(child) => {
                    if let Some(id) = &child.id {
                        ids.insert(id.clone());
                    }
                }
                _ => {}
            }
        }
    }
    for child in children {
        match child {
            RootChild::State(child) => collect_state(child, ids),
            RootChild::Final(child) => {
                if let Some(id) = &child.id {
                    ids.insert(id.clone());
                }
            }
            _ => {}
        }
    }
}

fn unique_name(base: &str, used: &mut HashSet<String>) -> String {
    if used.insert(base.to_owned()) {
        return base.to_owned();
    }
    let mut suffix = 0;
    loop {
        let candidate = format!("{base}{suffix}");
        suffix += 1;
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
}

fn has_foreign_attributes(attributes: &[Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        attribute.namespace.is_some()
            && attribute.name != "xmlns"
            && !attribute.name.starts_with("xmlns:")
    })
}

fn has_substantive_content(content: Option<&Content>) -> bool {
    content.is_some_and(|content| {
        !content.text.trim().is_empty()
            || content
                .parts
                .iter()
                .any(|part| matches!(part, ContentPart::Element(_)))
    })
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use super::*;
    use crate::scxml;
    use crate::{
        DatamodelOperation, InstanceStatus, LuauValue, RuntimeFault, StateTreeEvents,
        StateTreeExecution,
    };

    const NS: &str = super::super::SCXML_NAMESPACE;

    enum NoEvents {}

    impl StateTreeEvents for NoEvents {
        const AUTHORED_NAMES: &'static [&'static str] = &[];

        fn event_index(&self) -> usize {
            match *self {}
        }
    }

    #[test]
    fn compiles_hierarchy_and_precomputes_external_transition_paths() {
        let document = scxml::parse(
            "machine.scxml",
            &format!(
                r#"<scxml xmlns="{NS}" initial="parent">
                    <state id="parent" initial="first">
                        <state id="first"><transition event="advance" target="second"/></state>
                        <state id="second"/>
                    </state>
                </scxml>"#
            ),
        )
        .unwrap();

        let definition = compile(&document).unwrap();
        let compiled = &definition.compiled;
        let first = compiled
            .states
            .iter()
            .position(|state| state.name.as_ref() == "first")
            .unwrap();
        let second = compiled
            .states
            .iter()
            .position(|state| state.name.as_ref() == "second")
            .unwrap();
        let transition = compiled.states[first].transitions[0];

        assert_eq!(compiled.initial_entries.len(), 3);
        assert_eq!(transition.exit_depth, 2);
        assert_eq!(transition.entry_len, 1);
        assert_eq!(
            compiled.transition_entries[transition.entry_start as usize]
                .state
                .index(),
            second
        );
    }

    #[test]
    fn rejects_unsupported_semantics_instead_of_ignoring_them() {
        let document = scxml::parse(
            "condition.scxml",
            &format!(
                r#"<scxml xmlns="{NS}"><state id="first"><transition cond="true" target="second"/></state><state id="second"/></scxml>"#
            ),
        )
        .unwrap();

        let error = compile(&document).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("transition conditions require datamodel=\"luau\"")
        );
    }

    #[test]
    fn authored_descriptors_match_generated_completion_events() {
        let document = scxml::parse(
            "completion.scxml",
            &format!(
                r#"<scxml xmlns="{NS}" initial="parent">
                    <state id="parent" initial="finished">
                        <transition event="done.state.parent" target="pass"/>
                        <final id="finished"/>
                    </state>
                    <final id="pass"/>
                </scxml>"#
            ),
        )
        .unwrap();
        let definition = compile(&document).unwrap();

        let execution =
            StateTreeExecution::<NoEvents>::new(&definition, NonZeroUsize::new(8).unwrap());

        assert_eq!(execution.status(), &InstanceStatus::Complete);
        assert_eq!(execution.active_state_names().last(), Some("pass"));
    }

    fn execute(source: &str) -> StateTreeExecution<NoEvents> {
        let document = scxml::parse("raises.scxml", source).unwrap();
        let definition = compile(&document).unwrap();
        StateTreeExecution::<NoEvents>::new(&definition, NonZeroUsize::new(32).unwrap())
    }

    #[test]
    fn executes_initial_and_entry_raises_in_order() {
        let execution = execute(&format!(
            r#"<scxml xmlns="{NS}">
                <state id="parent">
                    <initial><transition target="leaf"><raise event="first"/></transition></initial>
                    <state id="leaf">
                        <onentry><raise event="second"/></onentry>
                        <transition event="first" target="waiting"/>
                    </state>
                    <state id="waiting"><transition event="second" target="pass"/></state>
                </state>
                <final id="pass"/>
            </scxml>"#
        ));

        assert_eq!(execution.status(), &InstanceStatus::Complete);
        assert_eq!(execution.active_state_names().last(), Some("pass"));
    }

    #[test]
    fn executes_exit_raises_before_transition_raises() {
        let execution = execute(&format!(
            r#"<scxml xmlns="{NS}">
                <state id="source">
                    <onentry><raise event="go"/></onentry>
                    <onexit><raise event="first"/></onexit>
                    <transition event="go" target="waiting"><raise event="second"/></transition>
                </state>
                <state id="waiting"><transition event="first" target="next"/></state>
                <state id="next"><transition event="second" target="pass"/></state>
                <final id="pass"/>
            </scxml>"#
        ));

        assert_eq!(execution.status(), &InstanceStatus::Complete);
        assert_eq!(execution.active_state_names().last(), Some("pass"));
    }

    #[test]
    fn final_entry_raises_precede_completion_events() {
        let execution = execute(&format!(
            r#"<scxml xmlns="{NS}">
                <state id="parent">
                    <transition event="first" target="waiting"/>
                    <final id="finished"><onentry><raise event="first"/></onentry></final>
                </state>
                <state id="waiting"><transition event="done.state.parent" target="pass"/></state>
                <final id="pass"/>
            </scxml>"#
        ));

        assert_eq!(execution.status(), &InstanceStatus::Complete);
        assert_eq!(execution.active_state_names().last(), Some("pass"));
    }

    #[test]
    fn direct_descendant_targets_do_not_execute_ancestor_initial_actions() {
        let execution = execute(&format!(
            r#"<scxml xmlns="{NS}">
                <state id="source"><transition target="leaf"/></state>
                <state id="parent">
                    <initial><transition target="other"><raise event="bad"/></transition></initial>
                    <state id="other"/>
                    <state id="leaf">
                        <onentry><raise event="good"/></onentry>
                        <transition event="good" target="pass"/>
                        <transition event="bad" target="fail"/>
                    </state>
                </state>
                <final id="pass"/>
                <final id="fail"/>
            </scxml>"#
        ));

        assert_eq!(execution.status(), &InstanceStatus::Complete);
        assert_eq!(execution.active_state_names().last(), Some("pass"));
    }

    #[test]
    fn luau_initializers_and_conditions_use_authored_order() {
        let execution = execute(&format!(
            r#"<scxml xmlns="{NS}" datamodel="luau">
                <datamodel>
                    <data id="base" expr="2"/>
                    <data id="threshold" expr="base * 3"/>
                </datamodel>
                <state id="source">
                    <transition cond="threshold == 6" target="pass"/>
                    <transition target="fail"/>
                </state>
                <final id="pass"/>
                <final id="fail"/>
            </scxml>"#
        ));

        assert_eq!(execution.status(), &InstanceStatus::Complete);
        assert_eq!(execution.active_state_names().last(), Some("pass"));
        assert_eq!(execution.data("threshold"), Some(&LuauValue::Integer(6)));
    }

    #[test]
    fn assignments_and_raises_preserve_executable_order() {
        let execution = execute(&format!(
            r#"<scxml xmlns="{NS}" datamodel="luau">
                <datamodel><data id="value" expr="0"/></datamodel>
                <state id="source">
                    <onentry>
                        <assign location="value" expr="1"/>
                        <raise event="verify"/>
                        <assign location="value" expr="2"/>
                    </onentry>
                    <transition event="verify" cond="value == 2" target="pass"/>
                    <transition event="verify" target="fail"/>
                </state>
                <final id="pass"/>
                <final id="fail"/>
            </scxml>"#
        ));

        assert_eq!(execution.status(), &InstanceStatus::Complete);
        assert_eq!(execution.active_state_names().last(), Some("pass"));
    }

    #[test]
    fn expression_data_and_assignments_allow_whitespace_content() {
        let execution = execute(&format!(
            r#"<scxml xmlns="{NS}" datamodel="luau">
                <datamodel><data id="value" expr="1">
                </data></datamodel>
                <state id="source">
                    <onentry><assign location="value" expr="value + 1">
                    </assign></onentry>
                    <transition cond="value == 2" target="pass"/>
                    <transition target="fail"/>
                </state>
                <final id="pass"/>
                <final id="fail"/>
            </scxml>"#
        ));

        assert_eq!(execution.status(), &InstanceStatus::Complete);
        assert_eq!(execution.active_state_names().last(), Some("pass"));
    }

    #[test]
    fn false_luau_guard_tries_the_next_authored_transition() {
        let execution = execute(&format!(
            r#"<scxml xmlns="{NS}" datamodel="luau">
                <state id="source">
                    <transition cond="false" target="fail"/>
                    <transition target="pass"/>
                </state>
                <final id="pass"/>
                <final id="fail"/>
            </scxml>"#
        ));

        assert_eq!(execution.status(), &InstanceStatus::Complete);
        assert_eq!(execution.active_state_names().last(), Some("pass"));
    }

    #[test]
    fn luau_runtime_errors_fault_the_instance() {
        let execution = execute(&format!(
            r#"<scxml xmlns="{NS}" datamodel="luau">
                <state id="source"><transition cond="missing()" target="pass"/></state>
                <final id="pass"/>
            </scxml>"#
        ));

        assert!(matches!(
            execution.status(),
            InstanceStatus::Faulted(RuntimeFault::Datamodel {
                operation: DatamodelOperation::Condition,
                ..
            })
        ));
    }

    #[test]
    fn rejects_invalid_luau_and_datamodel_declarations() {
        let invalid_expression = scxml::parse(
            "invalid.scxml",
            &format!(
                r#"<scxml xmlns="{NS}" datamodel="luau"><state id="source"><transition cond="(" target="pass"/></state><final id="pass"/></scxml>"#
            ),
        )
        .unwrap();
        assert!(matches!(
            compile(&invalid_expression),
            Err(CompileError::InvalidLuau { .. })
        ));

        let duplicate = scxml::parse(
            "duplicate.scxml",
            &format!(
                r#"<scxml xmlns="{NS}" datamodel="luau"><datamodel><data id="value"/><data id="value"/></datamodel><state id="source"/></scxml>"#
            ),
        )
        .unwrap();
        assert!(
            compile(&duplicate)
                .unwrap_err()
                .to_string()
                .contains("data id `value` is duplicated")
        );

        let reserved = scxml::parse(
            "reserved.scxml",
            &format!(
                r#"<scxml xmlns="{NS}" datamodel="luau"><datamodel><data id="_event"/></datamodel><state id="source"/></scxml>"#
            ),
        )
        .unwrap();
        assert!(
            compile(&reserved)
                .unwrap_err()
                .to_string()
                .contains("`_event` is reserved by the SCXML runtime")
        );
    }

    enum CounterEvent {
        Tick,
    }

    impl StateTreeEvents for CounterEvent {
        const AUTHORED_NAMES: &'static [&'static str] = &["tick"];

        fn event_index(&self) -> usize {
            match self {
                Self::Tick => 0,
            }
        }
    }

    #[test]
    fn instances_share_bytecode_but_not_luau_data() {
        let document = scxml::parse(
            "counter.scxml",
            &format!(
                r#"<scxml xmlns="{NS}" datamodel="luau">
                    <datamodel><data id="count" expr="0"/></datamodel>
                    <state id="running">
                        <transition event="tick" cond="_event.name == 'tick'" target="running">
                            <assign location="count" expr="count + 1"/>
                        </transition>
                    </state>
                </scxml>"#
            ),
        )
        .unwrap();
        let definition = compile(&document).unwrap();
        let mut first =
            StateTreeExecution::<CounterEvent>::new(&definition, NonZeroUsize::new(8).unwrap());
        let second =
            StateTreeExecution::<CounterEvent>::new(&definition, NonZeroUsize::new(8).unwrap());

        assert!(first.process_event(&CounterEvent::Tick, NonZeroUsize::new(8).unwrap()));

        assert_eq!(first.data("count"), Some(&LuauValue::Integer(1)));
        assert_eq!(second.data("count"), Some(&LuauValue::Integer(0)));

        let first = std::thread::spawn(move || {
            assert!(first.process_event(&CounterEvent::Tick, NonZeroUsize::new(8).unwrap()));
            first
        })
        .join()
        .unwrap();
        assert_eq!(first.data("count"), Some(&LuauValue::Integer(2)));
    }
}
