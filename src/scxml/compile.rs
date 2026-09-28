use std::collections::{HashMap, HashSet};

use thiserror::Error;

use super::{
    Attribute, Document, ExecutableContent, Final, FinalChild, Initial, OnEntry, OnExit, RootChild,
    Span, State, StateChild, Transition, TransitionType,
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
        let mut entry_raises = Vec::new();
        let mut exit_raises = Vec::new();
        for child in &state.children {
            match child {
                StateChild::OnEntry(value) => entry_raises.extend(self.entry_raises(value)?),
                StateChild::OnExit(value) => exit_raises.extend(self.exit_raises(value)?),
                _ => {}
            }
        }
        self.builder.entry_raises(runtime, entry_raises)?;
        self.builder.exit_raises(runtime, exit_raises)?;
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
        let mut entry_raises = Vec::new();
        let mut exit_raises = Vec::new();
        for child in &state.children {
            match child {
                FinalChild::OnEntry(value) => entry_raises.extend(self.entry_raises(value)?),
                FinalChild::OnExit(value) => exit_raises.extend(self.exit_raises(value)?),
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
        self.builder.entry_raises(runtime, entry_raises)?;
        self.builder.exit_raises(runtime, exit_raises)?;
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
            let raises = self.check_initial_transition(transition)?;
            self.builder.initial_raises(state, raises)?;
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
        &self,
        transition: &Transition,
    ) -> Result<Vec<Box<str>>, CompileError> {
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
        self.raises(&transition.content, transition.span)
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
        let raises = self.raises(&transition.content, transition.span)?;
        if has_foreign_attributes(&transition.attributes) {
            return self.unsupported(transition.span, "foreign transition attributes");
        }
        if transition.targets.len() != 1 {
            return self.unsupported(transition.span, "targetless or multi-target transitions");
        }
        let target = self.target(&transition.targets[0], transition.span)?;
        if transition.events.is_empty() {
            self.builder.eventless_with_raises(source, target, raises)?;
        } else {
            for event in &transition.events {
                self.builder.event_descriptor_with_raises(
                    source,
                    event.clone(),
                    target,
                    raises.clone(),
                )?;
            }
        }
        Ok(())
    }

    fn entry_raises(&self, entry: &OnEntry) -> Result<Vec<Box<str>>, CompileError> {
        if has_foreign_attributes(&entry.attributes) {
            return self.unsupported(entry.span, "foreign onentry attributes");
        }
        self.raises(&entry.content, entry.span)
    }

    fn exit_raises(&self, exit: &OnExit) -> Result<Vec<Box<str>>, CompileError> {
        if has_foreign_attributes(&exit.attributes) {
            return self.unsupported(exit.span, "foreign onexit attributes");
        }
        self.raises(&exit.content, exit.span)
    }

    fn raises(
        &self,
        content: &[ExecutableContent],
        span: Span,
    ) -> Result<Vec<Box<str>>, CompileError> {
        content
            .iter()
            .map(|executable| match executable {
                ExecutableContent::Raise(raise) if !has_foreign_attributes(&raise.attributes) => {
                    Ok(Box::<str>::from(raise.event.as_str()))
                }
                ExecutableContent::Raise(raise) => {
                    self.unsupported(raise.span, "foreign raise attributes")
                }
                _ => self.unsupported(span, "executable content other than raise"),
            })
            .collect()
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
}
