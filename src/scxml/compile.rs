use std::collections::{HashMap, HashSet};

use thiserror::Error;

use super::{
    Attribute, Document, Final, FinalChild, Initial, OnEntry, OnExit, RootChild, Span, State,
    StateChild, Transition, TransitionType,
};
use crate::{BuildError, StateId, StateTreeBuilder, StateTreeDefinition};

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
        }
    }

    fn compile(mut self) -> Result<StateTreeDefinition, CompileError> {
        self.reject_root_features()?;
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
            .is_some_and(|value| value != "null")
        {
            return self.unsupported(self.document.root.span, "datamodel selection");
        }
        if self.document.root.binding.is_some() {
            return self.unsupported(self.document.root.span, "datamodel binding");
        }
        if has_foreign_attributes(&self.document.root.attributes) {
            return self.unsupported(self.document.root.span, "foreign root attributes");
        }
        for child in &self.document.root.children {
            let (span, feature) = match child {
                RootChild::State(_) | RootChild::Final(_) => continue,
                RootChild::Parallel(value) => (value.span, "parallel states"),
                RootChild::DataModel(value) => (value.span, "datamodels"),
                RootChild::Script(value) => (value.span, "scripts"),
                RootChild::Extension(value) => (value.span, "foreign extensions"),
            };
            return self.unsupported(span, feature);
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
                StateChild::OnEntry(value) => self.check_entry(value)?,
                StateChild::OnExit(value) => self.check_exit(value)?,
                StateChild::Initial(_) | StateChild::Transition(_) => {}
            }
        }
        Ok(())
    }

    fn add_final(&mut self, state: &'a Final, parent: StateId) -> Result<(), CompileError> {
        if has_foreign_attributes(&state.attributes) {
            return self.unsupported(state.span, "foreign final-state attributes");
        }
        for child in &state.children {
            match child {
                FinalChild::OnEntry(value) => self.check_entry(value)?,
                FinalChild::OnExit(value) => self.check_exit(value)?,
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
            self.check_initial_transition(transition)?;
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

    fn check_initial_transition(&self, transition: &Transition) -> Result<(), CompileError> {
        if transition.targets.len() != 1 {
            return self.invalid_initial(transition.span);
        }
        if transition.event.is_some()
            || transition.condition.is_some()
            || transition.transition_type.is_some()
            || !transition.content.is_empty()
            || has_foreign_attributes(&transition.attributes)
        {
            return self.unsupported(transition.span, "executable initial transitions");
        }
        Ok(())
    }

    fn compile_transition(
        &mut self,
        source: StateId,
        transition: &Transition,
    ) -> Result<(), CompileError> {
        if transition.condition.is_some() {
            return self.unsupported(transition.span, "transition conditions");
        }
        if transition.transition_type == Some(TransitionType::Internal) {
            return self.unsupported(transition.span, "internal transitions");
        }
        if !transition.content.is_empty() {
            return self.unsupported(transition.span, "transition executable content");
        }
        if has_foreign_attributes(&transition.attributes) {
            return self.unsupported(transition.span, "foreign transition attributes");
        }
        if transition.targets.len() != 1 {
            return self.unsupported(transition.span, "targetless or multi-target transitions");
        }
        let target = self.target(&transition.targets[0], transition.span)?;
        if transition.events.is_empty() {
            self.builder.eventless(source, target)?;
        } else {
            for event in &transition.events {
                self.builder
                    .event_descriptor(source, event.clone(), target)?;
            }
        }
        Ok(())
    }

    fn check_entry(&self, entry: &OnEntry) -> Result<(), CompileError> {
        if entry.content.is_empty() && !has_foreign_attributes(&entry.attributes) {
            Ok(())
        } else {
            self.unsupported(entry.span, "onentry executable content")
        }
    }

    fn check_exit(&self, exit: &OnExit) -> Result<(), CompileError> {
        if exit.content.is_empty() && !has_foreign_attributes(&exit.attributes) {
            Ok(())
        } else {
            self.unsupported(exit.span, "onexit executable content")
        }
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

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use super::*;
    use crate::scxml;
    use crate::{InstanceStatus, StateTreeEvents, StateTreeExecution};

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
            compiled.transition_entries[transition.entry_start as usize].index(),
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

        assert!(error.to_string().contains("transition conditions"));
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
}
