use std::collections::{HashMap, HashSet};

use thiserror::Error;

use super::{
    Attribute, Content, ContentPart, DataModelChild, Document, ExecutableContent, Extension,
    ExtensionContent, Final, FinalChild, History, HistoryChild, Initial, OnEntry, OnExit, Parallel,
    ParallelChild, RootChild, Span, State, StateChild, Transition, TransitionType,
};
use crate::{
    BuildError, HistoryId, StateId, StateTreeAction, StateTreeActivityDefinition,
    StateTreeArgument, StateTreeBuilder, StateTreeData, StateTreeDefinition,
    StateTreeEffectArgument, StateTreeTarget, StateTreeTransition,
    definition::{is_activity_id, is_luau_identifier, is_scxml_system_name},
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
    #[error("{source_name}@{span_start}: invalid SCXML: {message}")]
    InvalidDocument {
        source_name: String,
        span_start: usize,
        message: String,
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
    #[error("{source_name}@{span_start}: invalid typed effect: {message}")]
    InvalidEffect {
        source_name: String,
        span_start: usize,
        message: String,
    },
    #[error("{source_name}@{span_start}: invalid activity: {message}")]
    InvalidActivity {
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
    Parallel(&'a Parallel),
    Final,
}

struct Record<'a> {
    node: Node<'a>,
    runtime: StateId,
    parent: StateId,
}

struct HistoryRecord<'a> {
    node: &'a History,
    owner: StateId,
    runtime: Option<HistoryId>,
}

struct Compiler<'a> {
    document: &'a Document,
    builder: StateTreeBuilder,
    records: Vec<Record<'a>>,
    histories: Vec<HistoryRecord<'a>>,
    ids: HashMap<&'a str, StateTreeTarget>,
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
            histories: Vec::new(),
            ids: HashMap::new(),
            used_names,
            root_name,
            next_anonymous: 0,
        }
    }

    fn compile(mut self) -> Result<StateTreeDefinition, CompileError> {
        self.reject_root_features()?;
        self.builder.domain_at(
            self.document
                .root
                .domain
                .as_deref()
                .expect("the domain declaration was validated"),
            self.document.root.span.start,
        );
        self.compile_datamodel()?;
        let root = self.builder.compound_root(self.root_name.clone())?;

        for child in &self.document.root.children {
            match child {
                RootChild::State(state) => self.add_state(state, root)?,
                RootChild::Parallel(state) => self.add_parallel(state, root)?,
                RootChild::Final(state) => self.add_final(state, root)?,
                _ => {}
            }
        }
        self.register_histories()?;
        self.compile_histories()?;
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
        match self.document.root.domain.as_deref() {
            None => {
                return self.invalid_document(
                    self.document.root.span,
                    "the root scxml element must declare `bst:domain`",
                );
            }
            Some("") => {
                return self.invalid_document(
                    self.document.root.span,
                    "the `bst:domain` declaration cannot be empty",
                );
            }
            Some(_) => {}
        }
        if let Some(version) = self.document.root.version.as_deref()
            && version != "1.0"
        {
            return self.invalid_document(
                self.document.root.span,
                format!("unsupported SCXML version `{version}`"),
            );
        }
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
        if has_unsupported_root_attributes(&self.document.root.attributes) {
            return self.unsupported(self.document.root.span, "foreign root attributes");
        }
        self.reject_unknown_attributes(
            self.document.root.span,
            "scxml",
            &self.document.root.attributes,
            &["name", "version", "initial", "binding", "datamodel"],
        )?;
        for child in &self.document.root.children {
            let (span, feature) = match child {
                RootChild::State(_)
                | RootChild::Parallel(_)
                | RootChild::Final(_)
                | RootChild::DataModel(_) => continue,
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
            let mut declarations = Vec::new();
            if let Some(model) = models.first() {
                if has_foreign_attributes(&model.attributes) {
                    return self.unsupported(model.span, "foreign datamodel attributes");
                }
                self.reject_unknown_attributes(model.span, "datamodel", &model.attributes, &[])?;
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
                    self.reject_unknown_attributes(
                        data.span,
                        "data",
                        &data.attributes,
                        &["id", "expr", "src"],
                    )?;
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
                    if declarations
                        .iter()
                        .any(|declaration: &StateTreeData| declaration.name() == id)
                    {
                        return self
                            .invalid_datamodel(data.span, format!("data id `{id}` is duplicated"));
                    }
                    let mut declaration = StateTreeData::new(id).at(data.span.start);
                    if let Some(expression) = data.expression.as_deref() {
                        declaration = declaration.with_initializer_at(expression, data.span.start);
                    }
                    declarations.push(declaration);
                }
            }
            let source_name = &self.document.source_name;
            self.builder
                .luau_datamodel(declarations)
                .map_err(|error| builder_error(source_name, error))?;
        }
        Ok(())
    }

    fn add_state(&mut self, state: &'a State, parent: StateId) -> Result<(), CompileError> {
        if has_foreign_attributes(&state.attributes) {
            return self.unsupported(state.span, "foreign state attributes");
        }
        self.reject_unknown_attributes(state.span, "state", &state.attributes, &["id", "initial"])?;
        let compound = state.children.iter().any(|child| {
            matches!(
                child,
                StateChild::State(_)
                    | StateChild::Parallel(_)
                    | StateChild::Final(_)
                    | StateChild::Initial(_)
            )
        });
        let name = self.node_name(state.id.as_deref());
        let runtime = if compound {
            self.builder.compound(name, parent)?
        } else {
            self.builder.atomic(name, parent)?
        };
        if let Some(id) = state.id.as_deref() {
            self.ids.insert(id, runtime.into());
        }
        let mut entry_actions = Vec::new();
        let mut exit_actions = Vec::new();
        for child in &state.children {
            match child {
                StateChild::OnEntry(value) => entry_actions.extend(self.entry_actions(value)?),
                StateChild::OnExit(value) => exit_actions.extend(self.exit_actions(value)?),
                StateChild::Extension(value) if is_activity(value) => {
                    let activity = self.activity(value)?;
                    let source_name = &self.document.source_name;
                    self.builder
                        .add_activity(runtime, activity)
                        .map_err(|error| builder_error(source_name, error))?;
                }
                _ => {}
            }
        }
        let source_name = &self.document.source_name;
        self.builder
            .entry_actions(runtime, entry_actions)
            .map_err(|error| builder_error(source_name, error))?;
        self.builder
            .exit_actions(runtime, exit_actions)
            .map_err(|error| builder_error(source_name, error))?;
        self.records.push(Record {
            node: Node::State(state),
            runtime,
            parent,
        });
        for child in &state.children {
            match child {
                StateChild::State(child) => self.add_state(child, runtime)?,
                StateChild::Final(child) => self.add_final(child, runtime)?,
                StateChild::Parallel(value) => self.add_parallel(value, runtime)?,
                StateChild::History(value) => self.histories.push(HistoryRecord {
                    node: value,
                    owner: runtime,
                    runtime: None,
                }),
                StateChild::DataModel(value) => {
                    return self.unsupported(value.span, "datamodels");
                }
                StateChild::Extension(value) if !is_activity(value) => {
                    return self.unsupported(value.span, "foreign extensions");
                }
                StateChild::Extension(_) => {}
                StateChild::OnEntry(_) | StateChild::OnExit(_) => {}
                StateChild::Initial(_) | StateChild::Transition(_) => {}
            }
        }
        Ok(())
    }

    fn add_parallel(
        &mut self,
        parallel: &'a Parallel,
        parent: StateId,
    ) -> Result<(), CompileError> {
        if has_foreign_attributes(&parallel.attributes) {
            return self.unsupported(parallel.span, "foreign parallel attributes");
        }
        self.reject_unknown_attributes(parallel.span, "parallel", &parallel.attributes, &["id"])?;
        let name = self.node_name(parallel.id.as_deref());
        let runtime = self.builder.parallel(name, parent)?;
        if let Some(id) = parallel.id.as_deref() {
            self.ids.insert(id, runtime.into());
        }
        let mut entry_actions = Vec::new();
        let mut exit_actions = Vec::new();
        for child in &parallel.children {
            match child {
                ParallelChild::OnEntry(value) => entry_actions.extend(self.entry_actions(value)?),
                ParallelChild::OnExit(value) => exit_actions.extend(self.exit_actions(value)?),
                ParallelChild::Extension(value) if is_activity(value) => {
                    let activity = self.activity(value)?;
                    let source_name = &self.document.source_name;
                    self.builder
                        .add_activity(runtime, activity)
                        .map_err(|error| builder_error(source_name, error))?;
                }
                _ => {}
            }
        }
        let source_name = &self.document.source_name;
        self.builder
            .entry_actions(runtime, entry_actions)
            .map_err(|error| builder_error(source_name, error))?;
        self.builder
            .exit_actions(runtime, exit_actions)
            .map_err(|error| builder_error(source_name, error))?;
        self.records.push(Record {
            node: Node::Parallel(parallel),
            runtime,
            parent,
        });
        for child in &parallel.children {
            match child {
                ParallelChild::State(child) => self.add_state(child, runtime)?,
                ParallelChild::Parallel(child) => self.add_parallel(child, runtime)?,
                ParallelChild::History(value) => self.histories.push(HistoryRecord {
                    node: value,
                    owner: runtime,
                    runtime: None,
                }),
                ParallelChild::DataModel(value) => {
                    return self.unsupported(value.span, "datamodels");
                }
                ParallelChild::Extension(value) if !is_activity(value) => {
                    return self.unsupported(value.span, "foreign extensions");
                }
                ParallelChild::Extension(_) => {}
                ParallelChild::OnEntry(_)
                | ParallelChild::OnExit(_)
                | ParallelChild::Transition(_) => {}
            }
        }
        Ok(())
    }

    fn add_final(&mut self, state: &'a Final, parent: StateId) -> Result<(), CompileError> {
        if has_foreign_attributes(&state.attributes) {
            return self.unsupported(state.span, "foreign final-state attributes");
        }
        self.reject_unknown_attributes(state.span, "final", &state.attributes, &["id"])?;
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
        let source_name = &self.document.source_name;
        self.builder
            .entry_actions(runtime, entry_actions)
            .map_err(|error| builder_error(source_name, error))?;
        self.builder
            .exit_actions(runtime, exit_actions)
            .map_err(|error| builder_error(source_name, error))?;
        if let Some(id) = state.id.as_deref() {
            self.ids.insert(id, runtime.into());
        }
        self.records.push(Record {
            node: Node::Final,
            runtime,
            parent,
        });
        Ok(())
    }

    fn register_histories(&mut self) -> Result<(), CompileError> {
        for index in 0..self.histories.len() {
            let history = self.histories[index].node;
            match history.history_type.as_deref() {
                None | Some("shallow") => {}
                Some("deep") => return self.unsupported(history.span, "deep history states"),
                Some(value) => {
                    return self
                        .invalid_document(history.span, format!("invalid history type `{value}`"));
                }
            }
            if has_foreign_attributes(&history.attributes) {
                return self.unsupported(history.span, "foreign history attributes");
            }
            self.reject_unknown_attributes(
                history.span,
                "history",
                &history.attributes,
                &["id", "type"],
            )?;
            if let Some(extension) = history.children.iter().find_map(|child| match child {
                HistoryChild::Extension(extension) => Some(extension),
                HistoryChild::Transition(_) => None,
            }) {
                return self.unsupported(extension.span, "foreign history extensions");
            }
            if history.children.len() != 1 {
                return self.invalid_document(
                    history.span,
                    "history requires exactly one transition child",
                );
            }

            let name = self.node_name(history.id.as_deref());
            let owner = self.histories[index].owner;
            let runtime = self.builder.shallow_history(name, owner).map_err(|error| {
                builder_error_at(&self.document.source_name, history.span, error)
            })?;
            self.histories[index].runtime = Some(runtime);
            if let Some(id) = history.id.as_deref() {
                self.ids.insert(id, runtime.into());
            }
        }
        Ok(())
    }

    fn compile_histories(&mut self) -> Result<(), CompileError> {
        for index in 0..self.histories.len() {
            let history = self.histories[index].node;
            let runtime = self.histories[index]
                .runtime
                .expect("histories are registered before defaults are compiled");
            let HistoryChild::Transition(transition) = &history.children[0] else {
                unreachable!("history extensions were rejected during registration")
            };
            self.reject_unknown_attributes(
                transition.span,
                "transition",
                &transition.attributes,
                &["event", "cond", "target", "type"],
            )?;
            if transition.event.is_some()
                || transition.condition.is_some()
                || transition.transition_type.is_some()
                || transition.after.is_some()
            {
                return self.unsupported(transition.span, "triggered history transitions");
            }
            if has_foreign_attributes(&transition.attributes) {
                return self.unsupported(transition.span, "foreign history transition attributes");
            }
            if transition.targets.len() != 1 {
                return self.invalid_document(
                    transition.span,
                    "history transition must have exactly one target",
                );
            }
            let target_name = &transition.targets[0];
            let StateTreeTarget::State(default) = self.target(target_name, transition.span)? else {
                return self.invalid_document(
                    transition.span,
                    "history default target must be a real state",
                );
            };
            let actions = self.actions(&transition.content, transition.span)?;
            self.builder
                .history_default(runtime, default)
                .map_err(|error| {
                    builder_error_at(&self.document.source_name, transition.span, error)
                })?;
            self.builder
                .history_actions(runtime, actions)
                .map_err(|error| {
                    builder_error_at(&self.document.source_name, transition.span, error)
                })?;
        }
        Ok(())
    }

    fn compile_record(&mut self, index: usize) -> Result<(), CompileError> {
        let node = self.records[index].node;
        let runtime = self.records[index].runtime;
        let Node::State(state) = node else {
            if let Node::Parallel(parallel) = node {
                for child in &parallel.children {
                    if let ParallelChild::Transition(transition) = child {
                        self.compile_transition(runtime, transition)?;
                    }
                }
            }
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
            self.reject_unknown_attributes(initial.span, "initial", &initial.attributes, &[])?;
            if !attribute.is_empty() {
                return self.unsupported(initial.span, "multiple initial declarations");
            }
            let Some(transition) = initial.transition.as_ref() else {
                return self.invalid_initial(initial.span);
            };
            let actions = self.check_initial_transition(transition)?;
            let source_name = &self.document.source_name;
            self.builder
                .initial_actions(state, actions)
                .map_err(|error| builder_error(source_name, error))?;
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
            child.into()
        };
        let target_span = explicit
            .and_then(|initial| initial.transition.as_ref())
            .map_or(span, |transition| transition.span);
        self.builder
            .initial(state, child)
            .map_err(|error| builder_error_at(&self.document.source_name, target_span, error))?;
        Ok(())
    }

    fn check_initial_transition(
        &mut self,
        transition: &Transition,
    ) -> Result<Vec<StateTreeAction>, CompileError> {
        self.reject_unknown_attributes(
            transition.span,
            "transition",
            &transition.attributes,
            &["event", "cond", "target", "type"],
        )?;
        if transition.targets.len() != 1 {
            return self.invalid_initial(transition.span);
        }
        if transition.event.is_some()
            || transition.after.is_some()
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
        self.reject_unknown_attributes(
            transition.span,
            "transition",
            &transition.attributes,
            &["event", "cond", "target", "type"],
        )?;
        let target_attribute = unqualified_attribute(&transition.attributes, "target");
        if transition
            .event
            .as_deref()
            .is_some_and(|event| event.trim().is_empty())
        {
            return self.invalid_document(transition.span, "transition event cannot be empty");
        }
        if transition.after.is_some() && transition.event.is_some() {
            return self.invalid_document(
                transition.span,
                "transition cannot combine bst:after with event",
            );
        }
        if target_attribute.is_some_and(|target| target.trim().is_empty()) {
            return self.invalid_document(transition.span, "transition target cannot be empty");
        }
        if transition.event.is_none()
            && transition.after.is_none()
            && transition.condition.is_none()
            && target_attribute.is_none()
        {
            return self.invalid_document(
                transition.span,
                "transition requires an event, condition, or target",
            );
        }
        if transition.condition.is_some() && self.document.root.datamodel.as_deref() != Some("luau")
        {
            return self.invalid_datamodel(
                transition.span,
                "transition conditions require datamodel=\"luau\"",
            );
        }
        let actions = self.actions(&transition.content, transition.span)?;
        if has_unsupported_transition_attributes(&transition.attributes) {
            return self.unsupported(transition.span, "foreign transition attributes");
        }
        if transition.targets.len() > 1 {
            return self.unsupported(transition.span, "multi-target transitions");
        }
        let target = transition
            .targets
            .first()
            .map(|target| self.target(target, transition.span))
            .transpose()?;
        let mut specification = if let Some(after) = transition.after {
            StateTreeTransition::after(after)
        } else if transition.events.is_empty() {
            StateTreeTransition::eventless()
        } else {
            StateTreeTransition::on_any(transition.events.iter().cloned())
        }
        .at(transition.span.start)
        .actions(actions);
        if let Some(condition) = transition.condition.as_deref() {
            specification = specification.condition_at(condition, transition.span.start);
        }
        if transition.transition_type == Some(TransitionType::Internal) {
            specification = specification.internal();
        }
        if let Some(target) = target {
            specification = specification.target(target);
        }
        let source_name = &self.document.source_name;
        self.builder
            .add_transition(source, specification)
            .map_err(|error| builder_error(source_name, error))?;
        Ok(())
    }

    fn entry_actions(&mut self, entry: &OnEntry) -> Result<Vec<StateTreeAction>, CompileError> {
        if has_foreign_attributes(&entry.attributes) {
            return self.unsupported(entry.span, "foreign onentry attributes");
        }
        self.reject_unknown_attributes(entry.span, "onentry", &entry.attributes, &[])?;
        self.actions(&entry.content, entry.span)
    }

    fn exit_actions(&mut self, exit: &OnExit) -> Result<Vec<StateTreeAction>, CompileError> {
        if has_foreign_attributes(&exit.attributes) {
            return self.unsupported(exit.span, "foreign onexit attributes");
        }
        self.reject_unknown_attributes(exit.span, "onexit", &exit.attributes, &[])?;
        self.actions(&exit.content, exit.span)
    }

    fn actions(
        &mut self,
        content: &[ExecutableContent],
        span: Span,
    ) -> Result<Vec<StateTreeAction>, CompileError> {
        content
            .iter()
            .map(|executable| match executable {
                ExecutableContent::Raise(raise) if !has_foreign_attributes(&raise.attributes) => {
                    self.reject_unknown_attributes(
                        raise.span,
                        "raise",
                        &raise.attributes,
                        &["event"],
                    )?;
                    if raise.event.trim().is_empty() {
                        return self
                            .invalid_document(raise.span, "raise requires a non-empty event");
                    }
                    Ok(StateTreeAction::raise(raise.event.as_str()))
                }
                ExecutableContent::Raise(raise) => {
                    self.unsupported(raise.span, "foreign raise attributes")
                }
                ExecutableContent::Assign(assign)
                    if !has_foreign_attributes(&assign.attributes)
                        && assign.source.is_none()
                        && !has_substantive_content(assign.content.as_ref()) =>
                {
                    self.reject_unknown_attributes(
                        assign.span,
                        "assign",
                        &assign.attributes,
                        &["location", "expr", "src"],
                    )?;
                    let Some(location) = assign.location.as_deref() else {
                        return self.invalid_datamodel(assign.span, "assign requires a location");
                    };
                    let Some(source) = assign.expression.as_deref() else {
                        return self
                            .invalid_datamodel(assign.span, "assign requires an expression");
                    };
                    Ok(StateTreeAction::assign_at(
                        location,
                        source,
                        assign.span.start,
                    ))
                }
                ExecutableContent::Assign(assign) => {
                    self.unsupported(assign.span, "non-expression Luau assignment")
                }
                ExecutableContent::Extension(extension)
                    if extension.namespace.as_deref() == Some(super::BEVY_STATE_TREE_NAMESPACE)
                        && extension.local_name == "effect" =>
                {
                    self.effect(extension)
                }
                _ => self.unsupported(span, "executable content other than raise"),
            })
            .collect()
    }

    fn effect(&mut self, extension: &Extension) -> Result<StateTreeAction, CompileError> {
        let name = extension_attribute(extension, "name")
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| self.effect_error(extension.span, "effect requires a non-empty name"))?;
        if extension
            .attributes
            .iter()
            .any(|attribute| attribute.namespace.is_some() || attribute.local_name != "name")
        {
            return Err(self.effect_error(extension.span, "effect has unsupported attributes"));
        }

        let mut arguments = Vec::new();
        let mut argument_names = HashSet::new();
        for content in &extension.content {
            let ExtensionContent::Element(argument) = content else {
                let ExtensionContent::Text(text) = content else {
                    unreachable!()
                };
                if !text.trim().is_empty() {
                    return Err(self.effect_error(extension.span, "effect contains text"));
                }
                continue;
            };
            if argument.namespace.as_deref() != Some(super::BEVY_STATE_TREE_NAMESPACE)
                || argument.local_name != "arg"
            {
                return Err(
                    self.effect_error(argument.span, "effect contains an unsupported child")
                );
            }
            if argument.content.iter().any(|content| match content {
                ExtensionContent::Text(text) => !text.trim().is_empty(),
                ExtensionContent::Element(_) => true,
            }) {
                return Err(self.effect_error(argument.span, "effect argument cannot have content"));
            }
            if argument.attributes.iter().any(|attribute| {
                attribute.namespace.is_some()
                    || !matches!(attribute.local_name.as_str(), "name" | "expr")
            }) {
                return Err(
                    self.effect_error(argument.span, "effect argument has unsupported attributes")
                );
            }
            let argument_name = extension_attribute(argument, "name")
                .filter(|name| !name.trim().is_empty())
                .ok_or_else(|| {
                    self.effect_error(argument.span, "effect argument requires a non-empty name")
                })?;
            if !argument_names.insert(argument_name) {
                return Err(self.effect_error(argument.span, "effect argument name is duplicated"));
            }
            let expression = extension_attribute(argument, "expr")
                .filter(|expression| !expression.trim().is_empty())
                .ok_or_else(|| {
                    self.effect_error(argument.span, "effect argument requires an expression")
                })?;
            if self.document.root.datamodel.as_deref() != Some("luau") {
                return Err(
                    self.effect_error(argument.span, "effect arguments require datamodel=\"luau\"")
                );
            }
            arguments.push(StateTreeEffectArgument::at(
                argument_name,
                expression,
                argument.span.start,
            ));
        }

        Ok(StateTreeAction::effect_at(
            name,
            arguments,
            extension.span.start,
        ))
    }

    fn activity(
        &mut self,
        extension: &Extension,
    ) -> Result<StateTreeActivityDefinition, CompileError> {
        let id = extension_attribute(extension, "id").ok_or_else(|| {
            self.activity_error(extension.span, "activity requires a non-empty id")
        })?;
        if id.is_empty() {
            return Err(self.activity_error(extension.span, "activity requires a non-empty id"));
        }
        if !is_activity_id(id) {
            return Err(self.activity_error(
                extension.span,
                format!(
                    "activity id `{id}` must be one event-name segment containing only ASCII letters, digits, `_`, or `-`"
                ),
            ));
        }
        let name = extension_attribute(extension, "name")
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                self.activity_error(extension.span, "activity requires a non-empty name")
            })?;
        if extension.attributes.iter().any(|attribute| {
            attribute.namespace.is_some() || !matches!(attribute.local_name.as_str(), "id" | "name")
        }) {
            return Err(self.activity_error(extension.span, "activity has unsupported attributes"));
        }
        let mut arguments = Vec::new();
        let mut names = HashSet::new();
        for content in &extension.content {
            let ExtensionContent::Element(argument) = content else {
                let ExtensionContent::Text(text) = content else {
                    unreachable!()
                };
                if !text.trim().is_empty() {
                    return Err(self.activity_error(extension.span, "activity contains text"));
                }
                continue;
            };
            if argument.namespace.as_deref() != Some(super::BEVY_STATE_TREE_NAMESPACE)
                || argument.local_name != "arg"
            {
                return Err(
                    self.activity_error(argument.span, "activity contains an unsupported child")
                );
            }
            if argument.content.iter().any(|content| match content {
                ExtensionContent::Text(text) => !text.trim().is_empty(),
                ExtensionContent::Element(_) => true,
            }) {
                return Err(
                    self.activity_error(argument.span, "activity argument cannot have content")
                );
            }
            if argument.attributes.iter().any(|attribute| {
                attribute.namespace.is_some()
                    || !matches!(attribute.local_name.as_str(), "name" | "expr")
            }) {
                return Err(self.activity_error(
                    argument.span,
                    "activity argument has unsupported attributes",
                ));
            }
            let argument_name = extension_attribute(argument, "name")
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| {
                    self.activity_error(
                        argument.span,
                        "activity argument requires a non-empty name",
                    )
                })?;
            if !names.insert(argument_name) {
                return Err(
                    self.activity_error(argument.span, "activity argument name is duplicated")
                );
            }
            let expression = extension_attribute(argument, "expr")
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| {
                    self.activity_error(argument.span, "activity argument requires an expression")
                })?;
            if self.document.root.datamodel.as_deref() != Some("luau") {
                return Err(self.activity_error(
                    argument.span,
                    "activity arguments require datamodel=\"luau\"",
                ));
            }
            arguments.push(StateTreeArgument::at(
                argument_name,
                expression,
                argument.span.start,
            ));
        }
        Ok(StateTreeActivityDefinition::new(id, name)
            .arguments(arguments)
            .at(extension.span.start))
    }

    fn target(&self, target: &str, span: Span) -> Result<StateTreeTarget, CompileError> {
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

    fn invalid_document<T>(
        &self,
        span: Span,
        message: impl Into<String>,
    ) -> Result<T, CompileError> {
        Err(CompileError::InvalidDocument {
            source_name: self.document.source_name.clone(),
            span_start: span.start,
            message: message.into(),
        })
    }

    fn reject_unknown_attributes(
        &self,
        span: Span,
        element: &str,
        attributes: &[Attribute],
        allowed: &[&str],
    ) -> Result<(), CompileError> {
        let Some(attribute) = attributes.iter().find(|attribute| {
            attribute.namespace.is_none()
                && attribute.name != "xmlns"
                && !allowed.contains(&attribute.local_name.as_str())
        }) else {
            return Ok(());
        };
        self.invalid_document(
            span,
            format!(
                "unknown attribute `{}` on <{element}>",
                attribute.local_name
            ),
        )
    }

    fn effect_error(&self, span: Span, message: impl Into<String>) -> CompileError {
        CompileError::InvalidEffect {
            source_name: self.document.source_name.clone(),
            span_start: span.start,
            message: message.into(),
        }
    }

    fn activity_error(&self, span: Span, message: impl Into<String>) -> CompileError {
        CompileError::InvalidActivity {
            source_name: self.document.source_name.clone(),
            span_start: span.start,
            message: message.into(),
        }
    }
}

fn is_activity(extension: &Extension) -> bool {
    extension.namespace.as_deref() == Some(super::BEVY_STATE_TREE_NAMESPACE)
        && extension.local_name == "activity"
}

fn builder_error(source_name: &str, error: BuildError) -> CompileError {
    match error {
        BuildError::InvalidLuau {
            span_start,
            message,
        } => CompileError::InvalidLuau {
            source_name: source_name.to_owned(),
            span_start,
            message: message.into(),
        },
        BuildError::UnknownDataName { name, span_start } => CompileError::InvalidDataModel {
            source_name: source_name.to_owned(),
            span_start,
            message: format!("assignment location `{name}` is not declared"),
        },
        error => CompileError::Build(error),
    }
}

fn builder_error_at(source_name: &str, span: Span, error: BuildError) -> CompileError {
    match builder_error(source_name, error) {
        CompileError::Build(error) => CompileError::InvalidDocument {
            source_name: source_name.to_owned(),
            span_start: span.start,
            message: error.to_string(),
        },
        error => error,
    }
}

fn extension_attribute<'a>(extension: &'a Extension, name: &str) -> Option<&'a str> {
    extension
        .attributes
        .iter()
        .find(|attribute| attribute.namespace.is_none() && attribute.local_name == name)
        .map(|attribute| attribute.value.as_str())
}

fn unqualified_attribute<'a>(attributes: &'a [Attribute], name: &str) -> Option<&'a str> {
    attributes
        .iter()
        .find(|attribute| attribute.namespace.is_none() && attribute.local_name == name)
        .map(|attribute| attribute.value.as_str())
}

fn collect_ids(children: &[RootChild], ids: &mut HashSet<String>) {
    fn collect_parallel(current: &Parallel, ids: &mut HashSet<String>) {
        if let Some(id) = &current.id {
            ids.insert(id.clone());
        }
        for child in &current.children {
            match child {
                ParallelChild::State(child) => collect_state(child, ids),
                ParallelChild::Parallel(child) => collect_parallel(child, ids),
                ParallelChild::History(child) => {
                    if let Some(id) = &child.id {
                        ids.insert(id.clone());
                    }
                }
                _ => {}
            }
        }
    }

    fn collect_state(current: &State, ids: &mut HashSet<String>) {
        if let Some(id) = &current.id {
            ids.insert(id.clone());
        }
        for child in &current.children {
            match child {
                StateChild::State(child) => collect_state(child, ids),
                StateChild::Parallel(child) => collect_parallel(child, ids),
                StateChild::Final(child) => {
                    if let Some(id) = &child.id {
                        ids.insert(id.clone());
                    }
                }
                StateChild::History(child) => {
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
            RootChild::Parallel(child) => collect_parallel(child, ids),
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

fn has_unsupported_root_attributes(attributes: &[Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        attribute.namespace.is_some()
            && attribute.name != "xmlns"
            && !attribute.name.starts_with("xmlns:")
            && !(attribute.namespace.as_deref() == Some(super::BEVY_STATE_TREE_NAMESPACE)
                && attribute.local_name == "domain")
    })
}

fn has_unsupported_transition_attributes(attributes: &[Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        attribute.namespace.is_some()
            && attribute.name != "xmlns"
            && !attribute.name.starts_with("xmlns:")
            && !(attribute.namespace.as_deref() == Some(super::BEVY_STATE_TREE_NAMESPACE)
                && attribute.local_name == "after")
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

    use bevy_ecs::{
        message::Message,
        system::{In, RunSystemOnce},
    };

    use super::*;
    use crate::scxml;
    use crate::{
        DatamodelOperation, InstanceStatus, LuauValue, RuntimeFault, StateTreeActivity,
        StateTreeActivityLifecycle, StateTreeActivityOutcomeWriter, StateTreeActivityToken,
        StateTreeDomain, StateTreeLinkFailure, test_harness::TestApp,
    };

    const NS: &str = super::super::SCXML_NAMESPACE;

    struct NoEventsDomain;

    impl StateTreeDomain for NoEventsDomain {
        const AUTHORED_NAME: &'static str = "test";

        fn configure(config: &mut crate::StateTreeConfig<'_, Self>) {
            config
                .effect::<RecordedEffect, _>("record", |_context, arguments| {
                    Ok(RecordedEffect(arguments.take("value")?))
                })
                .activity::<Work, _>("work", |context, arguments| {
                    let argument_name =
                        { arguments.iter().next().map(|(name, _)| name.to_owned()) };
                    let argument = argument_name
                        .map(|name| arguments.take::<LuauValue>(&name))
                        .transpose()?
                        .unwrap_or(LuauValue::Nil);
                    Ok(Work {
                        argument,
                        target: context.data("target").cloned(),
                    })
                })
                .activity::<Work, _>("find_path", |context, arguments| {
                    let argument = arguments.take("destination")?;
                    Ok(Work {
                        argument,
                        target: context.data("target").cloned(),
                    })
                });
        }
    }

    #[test]
    fn requires_a_nonempty_namespaced_domain_declaration() {
        let missing = scxml::parse(
            "missing-domain.scxml",
            &format!(r#"<scxml xmlns="{NS}"><state/></scxml>"#),
        )
        .unwrap();
        assert!(
            compile(&missing)
                .unwrap_err()
                .to_string()
                .contains("must declare `bst:domain`")
        );

        let empty = scxml::parse(
            "empty-domain.scxml",
            &format!(
                r#"<scxml xmlns="{NS}" xmlns:bst="{}" bst:domain=""><state/></scxml>"#,
                super::super::BEVY_STATE_TREE_NAMESPACE,
            ),
        )
        .unwrap();
        assert!(
            compile(&empty)
                .unwrap_err()
                .to_string()
                .contains("cannot be empty")
        );

        let unqualified = scxml::parse(
            "unqualified-domain.scxml",
            &format!(r#"<scxml xmlns="{NS}" domain="test"><state/></scxml>"#),
        )
        .unwrap();
        assert!(
            compile(&unqualified)
                .unwrap_err()
                .to_string()
                .contains("must declare `bst:domain`")
        );
    }

    #[derive(Message)]
    struct RecordedEffect(i64);

    #[derive(Clone, Debug)]
    struct Work {
        argument: LuauValue,
        target: Option<LuauValue>,
    }

    impl StateTreeActivity for Work {
        type Output = Option<LuauValue>;
        type Error = Option<LuauValue>;

        fn output_data(
            output: &Self::Output,
        ) -> Result<Option<LuauValue>, crate::StateTreeEventDataError> {
            Ok(output.clone())
        }

        fn error_data(
            error: &Self::Error,
        ) -> Result<Option<LuauValue>, crate::StateTreeEventDataError> {
            Ok(error.clone())
        }
    }

    type ActivityStarts<D> = Vec<(StateTreeActivityToken<D, Work>, Work)>;
    type ActivityCancellations<D> = Vec<StateTreeActivityToken<D, Work>>;

    fn drain_activities<D: StateTreeDomain>(
        execution: &mut TestApp<D>,
    ) -> (ActivityStarts<D>, ActivityCancellations<D>) {
        let mut starts = Vec::new();
        let mut cancellations = Vec::new();
        for lifecycle in execution
            .app_mut()
            .world_mut()
            .resource_mut::<bevy_ecs::message::Messages<StateTreeActivityLifecycle<D, Work>>>()
            .drain()
        {
            match lifecycle {
                StateTreeActivityLifecycle::Started { token, activity } => {
                    starts.push((token, activity));
                }
                StateTreeActivityLifecycle::Cancelled { token } => cancellations.push(token),
            }
        }
        (starts, cancellations)
    }

    fn complete_activity<D: StateTreeDomain>(
        execution: &mut TestApp<D>,
        token: StateTreeActivityToken<D, Work>,
        data: Option<LuauValue>,
    ) -> bool {
        let accepted = execution
            .app_mut()
            .world_mut()
            .run_system_once_with(write_completion::<D>, (token, data))
            .unwrap();
        execution
            .app_mut()
            .world_mut()
            .run_schedule(bevy_app::Update);
        accepted
    }

    fn write_completion<D: StateTreeDomain>(
        In((token, data)): In<(StateTreeActivityToken<D, Work>, Option<LuauValue>)>,
        mut outcomes: StateTreeActivityOutcomeWriter<D, Work>,
    ) -> bool {
        outcomes.complete(token, data)
    }

    #[test]
    fn compiles_hierarchy_and_precomputes_external_transition_paths() {
        let document = scxml::parse(
            "machine.scxml",
            &format!(
                r#"<scxml xmlns="{NS}" xmlns:bst="urn:bevy-state-tree" bst:domain="test" initial="parent">
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
    fn compiles_shallow_history_for_compound_and_parallel_owners() {
        let document = scxml::parse(
            "history.scxml",
            &format!(
                r#"<scxml xmlns="{NS}" xmlns:bst="urn:bevy-state-tree" bst:domain="test" initial="owner">
                    <state id="owner" initial="recent">
                        <state id="first"/><state id="second"/>
                        <history id="recent" type="shallow">
                            <transition target="first"><raise event="history.default"/></transition>
                        </history>
                    </state>
                    <state id="outside"><transition event="return" target="recent"/></state>
                    <parallel id="regions">
                        <state id="left"/><state id="right"/>
                        <history id="regions-recent"><transition target="left"/></history>
                    </parallel>
                </scxml>"#
            ),
        )
        .unwrap();

        let definition = compile(&document).unwrap();
        let compiled = &definition.compiled;
        let owner = compiled
            .states
            .iter()
            .position(|state| state.name.as_ref() == "owner")
            .unwrap();
        let outside = compiled
            .states
            .iter()
            .position(|state| state.name.as_ref() == "outside")
            .unwrap();

        assert_eq!(definition.history_count(), 2);
        assert_eq!(compiled.histories[0].owner.index(), owner);
        assert_eq!(compiled.histories[0].action_len, 1);
        let Some(StateTreeTarget::History(recent)) = compiled.states[outside].transitions[0].target
        else {
            panic!("return transition did not target history");
        };
        assert_eq!(definition.history_name(recent), Some("recent"));
        assert!(matches!(
            compiled.states[owner].kind,
            crate::definition::StateKind::Compound {
                initial: StateTreeTarget::History(_),
                ..
            }
        ));
    }

    #[test]
    fn anonymous_history_names_avoid_authored_ids() {
        let document = scxml::parse(
            "anonymous-history.scxml",
            &format!(
                r##"<scxml xmlns="{NS}" xmlns:bst="urn:bevy-state-tree" bst:domain="test"><state id="owner"><state id="#anonymous0"/><state/><history><transition target="#anonymous0"/></history></state></scxml>"##
            ),
        )
        .unwrap();

        let definition = compile(&document).unwrap();

        assert_eq!(
            definition.compiled.histories[0].name.as_ref(),
            "#anonymous2"
        );
    }

    #[test]
    fn rejects_deep_history_precisely() {
        let document = scxml::parse(
            "deep-history.scxml",
            &format!(
                r#"<scxml xmlns="{NS}" xmlns:bst="urn:bevy-state-tree" bst:domain="test"><state id="owner"><state id="child"/><history type="deep"><transition target="child"/></history></state></scxml>"#
            ),
        )
        .unwrap();

        assert!(matches!(
            compile(&document),
            Err(CompileError::Unsupported {
                feature: "deep history states",
                ..
            })
        ));
    }

    #[test]
    fn rejects_invalid_shallow_history_forms() {
        let namespace = super::super::BEVY_STATE_TREE_NAMESPACE;
        let cases = [
            (
                r#"<state id="owner"><state id="child"/><history/></state>"#,
                "history requires exactly one transition child",
            ),
            (
                r#"<state id="owner"><state id="child"/><history><transition target="child"/><transition target="child"/></history></state>"#,
                "history requires exactly one transition child",
            ),
            (
                r#"<state id="owner"><state id="child"/><history><transition/></history></state>"#,
                "history transition must have exactly one target",
            ),
            (
                r#"<state id="owner"><state id="child"/><history><transition target="child child"/></history></state>"#,
                "history transition must have exactly one target",
            ),
            (
                r#"<state id="owner"><state id="child"/><history><transition event="go" target="child"/></history></state>"#,
                "triggered history transitions",
            ),
            (
                r#"<state id="owner"><state id="child"/><history><transition cond="true" target="child"/></history></state>"#,
                "triggered history transitions",
            ),
            (
                r#"<state id="owner"><state id="child"/><history><transition type="internal" target="child"/></history></state>"#,
                "triggered history transitions",
            ),
            (
                r#"<state id="owner"><state id="child"/><history><transition bst:after="1s" target="child"/></history></state>"#,
                "triggered history transitions",
            ),
            (
                r#"<state id="owner"><state id="child"/><history other="value"><transition target="child"/></history></state>"#,
                "unknown attribute `other` on <history>",
            ),
            (
                r#"<state id="owner"><state id="child"/><history x:other="value"><transition target="child"/></history></state>"#,
                "foreign history attributes",
            ),
            (
                r#"<state id="owner"><state id="child"/><history><x:other/><transition target="child"/></history></state>"#,
                "foreign history extensions",
            ),
            (
                r#"<state id="owner"><history><transition target="owner"/></history></state>"#,
                "is not compound or parallel and cannot own history",
            ),
            (
                r#"<state id="owner"><state id="child"/><history id="first"><transition target="second"/></history><history id="second"><transition target="child"/></history></state>"#,
                "history default target must be a real state",
            ),
            (
                r#"<state id="owner"><state id="child"/><history><transition target="outside"/></history></state><state id="outside"/>"#,
                "is not a direct child",
            ),
        ];

        for (body, expected) in cases {
            let source = format!(
                r#"<scxml xmlns="{NS}" bst:domain="test" xmlns:bst="{namespace}" xmlns:x="urn:other">{body}</scxml>"#
            );
            let document = scxml::parse("invalid-history.scxml", &source).unwrap();
            let message = compile(&document).unwrap_err().to_string();
            assert!(
                message.contains(expected),
                "{message:?} did not contain {expected:?}"
            );
        }
    }

    #[test]
    fn rejects_unsupported_semantics_instead_of_ignoring_them() {
        let document = scxml::parse(
            "condition.scxml",
            &format!(
                r#"<scxml xmlns="{NS}" xmlns:bst="urn:bevy-state-tree" bst:domain="test"><state id="first"><transition cond="true" target="second"/></state><state id="second"/></scxml>"#
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
    fn rejects_malformed_supported_syntax() {
        let cases = [
            (
                r#"<scxml xmlns="{NS}" xmlns:bst="urn:bevy-state-tree" bst:domain="test"><state><transition/></state></scxml>"#,
                "transition requires an event, condition, or target",
            ),
            (
                r#"<scxml xmlns="{NS}" xmlns:bst="urn:bevy-state-tree" bst:domain="test"><state><transition event=" "/></state></scxml>"#,
                "transition event cannot be empty",
            ),
            (
                r#"<scxml xmlns="{NS}" xmlns:bst="urn:bevy-state-tree" bst:domain="test"><state><transition target=" "/></state></scxml>"#,
                "transition target cannot be empty",
            ),
            (
                r#"<scxml xmlns="{NS}" xmlns:bst="urn:bevy-state-tree" bst:domain="test"><state><onentry><raise/></onentry></state></scxml>"#,
                "raise requires a non-empty event",
            ),
            (
                r#"<scxml xmlns="{NS}" xmlns:bst="urn:bevy-state-tree" bst:domain="test"><state targett="typo"/></scxml>"#,
                "unknown attribute `targett` on <state>",
            ),
            (
                r#"<scxml xmlns="{NS}" xmlns:bst="urn:bevy-state-tree" bst:domain="test" version="2.0"><state/></scxml>"#,
                "unsupported SCXML version `2.0`",
            ),
        ];

        for (source, expected) in cases {
            let document = scxml::parse("invalid.scxml", &source.replace("{NS}", NS)).unwrap();
            assert!(
                compile(&document)
                    .unwrap_err()
                    .to_string()
                    .contains(expected)
            );
        }
    }

    #[test]
    fn rejects_event_and_after_on_the_same_transition() {
        let source = format!(
            r#"<scxml xmlns="{NS}" bst:domain="test" xmlns:bst="{}"><state><transition event="go" bst:after="1s"/></state></scxml>"#,
            super::super::BEVY_STATE_TREE_NAMESPACE,
        );
        let document = scxml::parse("event-after.scxml", &source).unwrap();

        assert!(
            compile(&document)
                .unwrap_err()
                .to_string()
                .contains("transition cannot combine bst:after with event")
        );
    }

    #[test]
    fn after_does_not_allow_other_foreign_transition_attributes() {
        let source = format!(
            r#"<scxml xmlns="{NS}" bst:domain="test" xmlns:bst="{}" xmlns:x="urn:other"><state><transition bst:after="1s" x:after="2s"/></state></scxml>"#,
            super::super::BEVY_STATE_TREE_NAMESPACE,
        );
        let document = scxml::parse("foreign-after.scxml", &source).unwrap();

        assert!(matches!(
            compile(&document),
            Err(CompileError::Unsupported {
                feature: "foreign transition attributes",
                ..
            })
        ));
    }

    #[test]
    fn unqualified_after_is_not_the_bevy_extension() {
        let source = format!(
            r#"<scxml xmlns="{NS}" xmlns:bst="urn:bevy-state-tree" bst:domain="test"><state><transition after="1s" target="done"/></state><final id="done"/></scxml>"#,
        );
        let document = scxml::parse("unqualified-after.scxml", &source).unwrap();

        assert!(
            compile(&document)
                .unwrap_err()
                .to_string()
                .contains("unknown attribute `after` on <transition>")
        );
    }

    #[test]
    fn authored_descriptors_match_generated_completion_events() {
        let document = scxml::parse(
            "completion.scxml",
            &format!(
                r#"<scxml xmlns="{NS}" xmlns:bst="urn:bevy-state-tree" bst:domain="test" initial="parent">
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

        let execution = TestApp::<NoEventsDomain>::new(definition, NonZeroUsize::new(8).unwrap());

        assert_eq!(execution.status(), &InstanceStatus::Complete);
        assert_eq!(execution.active_state_names().last(), Some("pass"));
    }

    #[test]
    fn linking_rejects_unmatched_external_event_descriptors() {
        let document = scxml::parse(
            "typo.scxml",
            &format!(
                r#"<scxml xmlns="{NS}" xmlns:bst="urn:bevy-state-tree" bst:domain="test"><state id="waiting"><transition event="adavnce" target="done"/></state><final id="done"/></scxml>"#
            ),
        )
        .unwrap();
        let definition = compile(&document).unwrap();

        let execution = TestApp::<NoEventsDomain>::new(definition, NonZeroUsize::new(8).unwrap());
        let error = execution
            .app()
            .world()
            .get::<StateTreeLinkFailure<NoEventsDomain>>(execution.entity())
            .unwrap()
            .error();

        assert!(matches!(
            &error.issues()[0],
            crate::StateTreeLinkIssue::UnmatchedEventDescriptor {
                descriptor,
                span_start: Some(_),
            } if descriptor.as_ref() == "adavnce"
        ));
    }

    #[test]
    fn rejects_wildcard_event_descriptors() {
        for descriptor in ["*", "combat.*"] {
            let document = scxml::parse(
                "wildcard.scxml",
                &format!(
                    r#"<scxml xmlns="{NS}" xmlns:bst="urn:bevy-state-tree" bst:domain="test"><state><transition event="{descriptor}"/></state></scxml>"#
                ),
            )
            .unwrap();

            assert!(
                compile(&document)
                    .unwrap_err()
                    .to_string()
                    .contains("must name one exact event")
            );
        }
    }

    #[test]
    fn root_completion_retains_final_without_running_its_exit_actions() {
        let document = scxml::parse(
            "complete.scxml",
            &format!(
                r#"<scxml xmlns="{NS}" xmlns:bst="urn:bevy-state-tree" bst:domain="test" datamodel="luau" initial="done">
                    <datamodel><data id="exited" expr="false"/></datamodel>
                    <final id="done"><onexit><assign location="exited" expr="true"/></onexit></final>
                </scxml>"#
            ),
        )
        .unwrap();
        let definition = compile(&document).unwrap();

        let execution = TestApp::<NoEventsDomain>::new(definition, NonZeroUsize::new(8).unwrap());

        assert_eq!(execution.status(), &InstanceStatus::Complete);
        assert_eq!(
            execution.active_state_names().collect::<Vec<_>>(),
            ["#document", "done"]
        );
        assert_eq!(execution.data("exited"), Some(&LuauValue::Boolean(false)));
    }

    fn execute(source: &str) -> TestApp<NoEventsDomain> {
        let document = scxml::parse("raises.scxml", source).unwrap();
        let definition = compile(&document).unwrap();
        TestApp::<NoEventsDomain>::new(definition, NonZeroUsize::new(32).unwrap())
    }

    #[test]
    fn compiles_and_emits_typed_effect_arguments() {
        let source = format!(
            r#"<scxml xmlns="{NS}" bst:domain="test" xmlns:bst="{}" datamodel="luau">
                <state id="source">
                    <onentry><bst:effect name="record"><bst:arg name="value" expr="2 + 3"/></bst:effect></onentry>
                </state>
            </scxml>"#,
            super::super::BEVY_STATE_TREE_NAMESPACE,
        );
        let document = scxml::parse("effect.scxml", &source).unwrap();
        let definition = compile(&document).unwrap();
        let mut execution =
            TestApp::<NoEventsDomain>::new(definition, NonZeroUsize::new(8).unwrap());

        assert_eq!(execution.status(), &InstanceStatus::Running);
        let effects = execution
            .app_mut()
            .world_mut()
            .resource_mut::<bevy_ecs::message::Messages<RecordedEffect>>()
            .drain()
            .map(|effect| effect.0)
            .collect::<Vec<_>>();
        assert_eq!(effects, [5]);
    }

    #[test]
    fn activity_syntax_requires_ids_names_and_valid_unique_arguments() {
        let namespace = super::super::BEVY_STATE_TREE_NAMESPACE;
        let cases = [
            (r#"<bst:activity name="work"/>"#, "non-empty id"),
            (r#"<bst:activity id="job"/>"#, "non-empty name"),
            (
                r#"<bst:activity id="job" name="work"><bst:arg name="x"/></bst:activity>"#,
                "requires an expression",
            ),
            (
                r#"<bst:activity id="job" name="work"><bst:arg name="x" expr="1"/><bst:arg name="x" expr="2"/></bst:activity>"#,
                "duplicated",
            ),
            (
                r#"<bst:activity id="job.child" name="work"/>"#,
                "event-name segment",
            ),
            (
                r#"<bst:activity id="*" name="work"/>"#,
                "event-name segment",
            ),
            (
                r#"<bst:activity id="job child" name="work"/>"#,
                "event-name segment",
            ),
        ];
        for (activity, expected) in cases {
            let source = format!(
                r#"<scxml xmlns="{NS}" bst:domain="test" xmlns:bst="{namespace}" datamodel="luau"><state>{activity}</state></scxml>"#
            );
            let document = scxml::parse("activity-invalid.scxml", &source).unwrap();
            assert!(
                compile(&document)
                    .unwrap_err()
                    .to_string()
                    .contains(expected)
            );
        }
    }

    #[test]
    fn activity_ids_accept_safe_event_name_segments() {
        let namespace = super::super::BEVY_STATE_TREE_NAMESPACE;
        for id in ["job", "job-child", "job_child"] {
            let source = format!(
                r#"<scxml xmlns="{NS}" bst:domain="test" xmlns:bst="{namespace}"><state><bst:activity id="{id}" name="work"/></state></scxml>"#
            );
            let document = scxml::parse("activity-valid.scxml", &source).unwrap();
            compile(&document).unwrap();
        }
    }

    #[test]
    fn valid_activity_ids_have_distinct_done_and_error_events() {
        let source = format!(
            r#"<scxml xmlns="{NS}" bst:domain="test" xmlns:bst="{}">
                <state>
                    <bst:activity id="job" name="work"/>
                    <bst:activity id="job-child" name="work"/>
                </state>
            </scxml>"#,
            super::super::BEVY_STATE_TREE_NAMESPACE,
        );
        let definition = compile(&scxml::parse("activity-prefix.scxml", &source).unwrap()).unwrap();
        let events = definition
            .compiled
            .activities
            .iter()
            .flat_map(|activity| [activity.done, activity.error])
            .map(|event| definition.compiled.event_name(event).unwrap())
            .collect::<Vec<_>>();

        assert_eq!(
            events,
            [
                "done.activity.job",
                "error.activity.job",
                "done.activity.job-child",
                "error.activity.job-child",
            ]
        );
    }

    #[test]
    fn app_activity_completion_carries_data_and_is_accepted_once() {
        let source = format!(
            r#"<scxml xmlns="{NS}" bst:domain="test" xmlns:bst="{}" datamodel="luau">
                <datamodel><data id="target" expr="'north'"/><data id="result"/></datamodel>
                <state id="active">
                    <onentry><assign location="target" expr="'south'"/></onentry>
                    <bst:activity id="path" name="find_path"><bst:arg name="destination" expr="target"/></bst:activity>
                    <transition event="done.activity.path" cond="_event.data.ok"><assign location="result" expr="_event.data.route"/></transition>
                </state>
            </scxml>"#,
            super::super::BEVY_STATE_TREE_NAMESPACE,
        );
        let definition = compile(&scxml::parse("activity.scxml", &source).unwrap()).unwrap();
        let mut execution =
            TestApp::<NoEventsDomain>::new(definition, NonZeroUsize::new(8).unwrap());
        let (starts, cancellations) = drain_activities(&mut execution);
        assert!(cancellations.is_empty());
        assert_eq!(starts[0].1.argument, LuauValue::String("south".into()));
        assert_eq!(starts[0].1.target, Some(LuauValue::String("south".into())));
        let token = starts[0].0;
        let data = LuauValue::Record(
            [
                (Box::from("ok"), LuauValue::Boolean(true)),
                (Box::from("route"), LuauValue::String("n1".into())),
            ]
            .into(),
        );

        assert!(complete_activity(&mut execution, token, Some(data)));
        assert_eq!(
            execution.data("result"),
            Some(&LuauValue::String("n1".into()))
        );
        assert_eq!(execution.active_state_names().last(), Some("active"));
        assert!(!complete_activity(&mut execution, token, None));
        assert!(drain_activities(&mut execution).1.is_empty());
    }

    #[test]
    fn unhandled_activity_outcome_still_stabilizes_eventless_transitions() {
        let source = format!(
            r#"<scxml xmlns="{NS}" bst:domain="test" xmlns:bst="{}" datamodel="luau">
                <state id="active">
                    <bst:activity id="job" name="work"/>
                    <transition cond="_event ~= nil and _event.data == 7" target="pass"/>
                </state>
                <final id="pass"/>
            </scxml>"#,
            super::super::BEVY_STATE_TREE_NAMESPACE,
        );
        let definition =
            compile(&scxml::parse("activity-eventless.scxml", &source).unwrap()).unwrap();
        let mut execution =
            TestApp::<NoEventsDomain>::new(definition, NonZeroUsize::new(1).unwrap());
        let token = drain_activities(&mut execution).0[0].0;

        assert!(complete_activity(
            &mut execution,
            token,
            Some(LuauValue::Integer(7))
        ));
        assert_eq!(execution.status(), &InstanceStatus::Complete);
        assert_eq!(execution.active_state_names().last(), Some("pass"));
    }

    #[test]
    fn activity_payload_does_not_leak_to_a_raised_internal_event() {
        let source = format!(
            r#"<scxml xmlns="{NS}" bst:domain="test" xmlns:bst="{}" datamodel="luau">
                <datamodel><data id="leaked" expr="true"/></datamodel>
                <state id="active">
                    <bst:activity id="job" name="work"/>
                    <transition event="done.activity.job" target="checking"><raise event="check"/></transition>
                </state>
                <state id="checking">
                    <transition event="check" cond="_event.data == nil" target="pass"/>
                    <transition event="check" target="fail"/>
                </state>
                <final id="pass"/><final id="fail"/>
            </scxml>"#,
            super::super::BEVY_STATE_TREE_NAMESPACE,
        );
        let definition = compile(&scxml::parse("payload-scope.scxml", &source).unwrap()).unwrap();
        let mut execution =
            TestApp::<NoEventsDomain>::new(definition, NonZeroUsize::new(8).unwrap());
        let token = drain_activities(&mut execution).0[0].0;

        assert!(complete_activity(
            &mut execution,
            token,
            Some(LuauValue::Integer(7))
        ));
        assert_eq!(execution.active_state_names().last(), Some("pass"));
    }

    #[test]
    fn app_effects_and_activities_support_mixed_macrosteps() {
        let source = format!(
            r#"<scxml xmlns="{NS}" bst:domain="test" xmlns:bst="{}" datamodel="luau">
                <state id="active">
                    <bst:activity id="finished" name="work"/>
                    <bst:activity id="cancelled" name="work"/>
                    <transition event="done.activity.finished" target="next">
                        <bst:effect name="record"><bst:arg name="value" expr="9"/></bst:effect>
                    </transition>
                </state>
                <state id="next"><bst:activity id="started" name="work"/></state>
            </scxml>"#,
            super::super::BEVY_STATE_TREE_NAMESPACE,
        );
        let definition = compile(&scxml::parse("mixed-sinks.scxml", &source).unwrap()).unwrap();
        let mut execution =
            TestApp::<NoEventsDomain>::new(definition, NonZeroUsize::new(8).unwrap());
        let (starts, _) = drain_activities(&mut execution);
        let completed = starts[0].0;
        let cancelled = starts[1].0;

        assert!(complete_activity(&mut execution, completed, None));
        let effects = execution
            .app_mut()
            .world_mut()
            .resource_mut::<bevy_ecs::message::Messages<RecordedEffect>>()
            .drain()
            .map(|effect| effect.0)
            .collect::<Vec<_>>();
        assert_eq!(effects, [9]);
        let (new_starts, cancellations) = drain_activities(&mut execution);
        assert_eq!(cancellations, [cancelled]);
        assert_eq!(new_starts.len(), 1);
        assert_eq!(execution.active_state_names().last(), Some("next"));
    }

    struct ReenterDomain;

    impl StateTreeDomain for ReenterDomain {
        const AUTHORED_NAME: &'static str = "test";

        fn configure(config: &mut crate::StateTreeConfig<'_, Self>) {
            config
                .event::<ReenterEvent>()
                .activity::<Work, _>("work", |_context, _arguments| {
                    Ok(Work {
                        argument: LuauValue::Nil,
                        target: None,
                    })
                });
        }
    }

    #[derive(bevy_ecs::event::Event, crate::StateTreeEvent)]
    #[state_tree(domain = ReenterDomain, name = "reenter", broadcast)]
    struct ReenterEvent;

    #[test]
    fn reentry_uses_a_new_generation_and_stale_outcomes_are_ignored() {
        let source = format!(
            r#"<scxml xmlns="{NS}" bst:domain="test" xmlns:bst="{}">
            <state id="active"><bst:activity id="job" name="work"/><transition event="reenter" target="active"/><transition event="done.activity.job"/></state>
        </scxml>"#,
            super::super::BEVY_STATE_TREE_NAMESPACE
        );
        let definition = compile(&scxml::parse("reentry.scxml", &source).unwrap()).unwrap();
        let mut execution =
            TestApp::<ReenterDomain>::new(definition, NonZeroUsize::new(8).unwrap());
        let first = drain_activities(&mut execution).0[0].0;
        assert!(execution.process_event(ReenterEvent));
        let (starts, cancellations) = drain_activities(&mut execution);
        let second = starts[0].0;
        assert_ne!(first, second);
        assert_eq!(cancellations, [first]);
        assert!(!complete_activity(&mut execution, first, None));
        assert!(complete_activity(&mut execution, second, None));
    }

    #[test]
    fn root_completion_cancels_still_running_activities() {
        let source = format!(
            r#"<scxml xmlns="{NS}" bst:domain="test" xmlns:bst="{}" initial="done">
            <bst:activity id="root-job" name="work"/><final id="done"/>
        </scxml>"#,
            super::super::BEVY_STATE_TREE_NAMESPACE
        );
        let document = scxml::parse("root-activity.scxml", &source).unwrap();
        // Activities are structural children of authored state/parallel nodes, not the synthetic document root.
        assert!(matches!(
            compile(&document),
            Err(CompileError::Unsupported { .. })
        ));

        let mut builder = StateTreeBuilder::new("root-complete");
        let root = builder.compound_root("root").unwrap();
        let done = builder.final_state("done", root).unwrap();
        builder.initial(root, done).unwrap();
        builder
            .add_activity(root, StateTreeActivityDefinition::new("root-job", "work"))
            .unwrap();
        let definition = builder.build().unwrap();
        let mut execution =
            TestApp::<NoEventsDomain>::new(definition, NonZeroUsize::new(8).unwrap());
        let (starts, cancellations) = drain_activities(&mut execution);
        assert_eq!(execution.status(), &InstanceStatus::Complete);
        assert_eq!(starts.len(), 1);
        assert_eq!(cancellations, [starts[0].0]);
    }

    #[test]
    fn parallel_activity_starts_follow_entry_document_order() {
        let source = format!(
            r#"<scxml xmlns="{NS}" bst:domain="test" xmlns:bst="{}">
            <parallel id="p">
                <bst:activity id="outer" name="work"/>
                <state id="left"><bst:activity id="left-job" name="work"/></state>
                <state id="right"><bst:activity id="right-job" name="work"/></state>
            </parallel>
        </scxml>"#,
            super::super::BEVY_STATE_TREE_NAMESPACE
        );
        let definition =
            compile(&scxml::parse("parallel-activity.scxml", &source).unwrap()).unwrap();
        let mut execution =
            TestApp::<NoEventsDomain>::new(definition, NonZeroUsize::new(8).unwrap());
        let (starts, _) = drain_activities(&mut execution);
        assert_eq!(execution.status(), &InstanceStatus::Running);
        assert_eq!(starts.len(), 3);
        assert!(
            starts
                .iter()
                .all(|(_, work)| work.argument == LuauValue::Nil)
        );
    }

    #[test]
    fn stabilization_fault_cancels_the_current_activity_run() {
        let mut builder = StateTreeBuilder::new("activity-fault");
        let root = builder.compound_root("root").unwrap();
        let active = builder.atomic("active", root).unwrap();
        builder.initial(root, active).unwrap();
        builder
            .add_activity(active, StateTreeActivityDefinition::new("job", "work"))
            .unwrap();
        builder
            .add_transition(active, StateTreeTransition::eventless().target(active))
            .unwrap();
        let definition = builder.build().unwrap();
        let mut execution =
            TestApp::<NoEventsDomain>::new(definition, NonZeroUsize::new(1).unwrap());
        let (starts, cancellations) = drain_activities(&mut execution);
        assert!(matches!(
            execution.status(),
            InstanceStatus::Faulted(RuntimeFault::MicrostepLimitExceeded { .. })
        ));
        assert_eq!(starts.len(), 2);
        assert_eq!(cancellations, [starts[0].0, starts[1].0]);
    }

    struct PayloadDomain;

    impl StateTreeDomain for PayloadDomain {
        const AUTHORED_NAME: &'static str = "test";

        fn configure(config: &mut crate::StateTreeConfig<'_, Self>) {
            config.event::<PayloadEvent>();
        }
    }

    #[derive(bevy_ecs::event::Event, crate::StateTreeEvent)]
    #[state_tree(
        domain = PayloadDomain,
        name = "payload",
        broadcast,
        data = payload_event_data
    )]
    struct PayloadEvent {
        amount: i64,
    }

    fn payload_event_data(
        event: &PayloadEvent,
    ) -> Result<LuauValue, crate::StateTreeEventDataError> {
        Ok(LuauValue::Record(
            [(Box::from("amount"), LuauValue::Integer(event.amount))].into(),
        ))
    }

    #[test]
    fn luau_conditions_can_read_projected_external_event_data() {
        let source = format!(
            r#"<scxml xmlns="{NS}" xmlns:bst="urn:bevy-state-tree" bst:domain="test" datamodel="luau">
                <state id="source">
                    <transition event="payload" cond="_event.data.amount == 7" target="pass"/>
                    <transition event="payload" target="fail"/>
                </state>
                <final id="pass"/><final id="fail"/>
            </scxml>"#,
        );
        let document = scxml::parse("payload.scxml", &source).unwrap();
        let definition = compile(&document).unwrap();
        let mut execution =
            TestApp::<PayloadDomain>::new(definition, NonZeroUsize::new(8).unwrap());

        assert!(execution.process_event(PayloadEvent { amount: 7 }));
        assert_eq!(execution.active_state_names().last(), Some("pass"));
    }

    struct FailedProjectionDomain;

    impl StateTreeDomain for FailedProjectionDomain {
        const AUTHORED_NAME: &'static str = "test";

        fn configure(config: &mut crate::StateTreeConfig<'_, Self>) {
            config.event::<FailedProjectionEvent>();
        }
    }

    #[derive(bevy_ecs::event::Event, crate::StateTreeEvent)]
    #[state_tree(
        domain = FailedProjectionDomain,
        name = "trigger",
        broadcast,
        data = failed_projection_event_data
    )]
    struct FailedProjectionEvent;

    fn failed_projection_event_data(
        _event: &FailedProjectionEvent,
    ) -> Result<LuauValue, crate::StateTreeEventDataError> {
        Err(crate::StateTreeEventDataError::new("projection failed"))
    }

    #[test]
    fn event_data_projection_errors_fault_the_instance() {
        let source = format!(
            r#"<scxml xmlns="{NS}" xmlns:bst="urn:bevy-state-tree" bst:domain="test" datamodel="luau">
                <state id="source"><transition event="trigger" cond="_event.data ~= nil" target="pass"/></state>
                <final id="pass"/>
            </scxml>"#,
        );
        let document = scxml::parse("projection.scxml", &source).unwrap();
        let definition = compile(&document).unwrap();
        let mut execution =
            TestApp::<FailedProjectionDomain>::new(definition, NonZeroUsize::new(8).unwrap());

        assert!(!execution.process_event(FailedProjectionEvent));
        assert!(matches!(
            execution.status(),
            InstanceStatus::Faulted(RuntimeFault::EventDataProjection { .. })
        ));
    }

    #[test]
    fn reading_only_event_name_does_not_project_payload_data() {
        let source = format!(
            r#"<scxml xmlns="{NS}" xmlns:bst="urn:bevy-state-tree" bst:domain="test" datamodel="luau">
                <state id="source"><transition event="trigger" cond="_event.name == 'trigger'" target="pass"/></state>
                <final id="pass"/>
            </scxml>"#,
        );
        let document = scxml::parse("event-name.scxml", &source).unwrap();
        let definition = compile(&document).unwrap();
        let mut execution =
            TestApp::<FailedProjectionDomain>::new(definition, NonZeroUsize::new(8).unwrap());

        assert!(execution.process_event(FailedProjectionEvent));
        assert_eq!(execution.active_state_names().last(), Some("pass"));
    }

    #[test]
    fn executes_initial_and_entry_raises_in_order() {
        let execution = execute(&format!(
            r#"<scxml xmlns="{NS}" xmlns:bst="urn:bevy-state-tree" bst:domain="test">
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
            r#"<scxml xmlns="{NS}" xmlns:bst="urn:bevy-state-tree" bst:domain="test">
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
            r#"<scxml xmlns="{NS}" xmlns:bst="urn:bevy-state-tree" bst:domain="test">
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
            r#"<scxml xmlns="{NS}" xmlns:bst="urn:bevy-state-tree" bst:domain="test">
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
            r#"<scxml xmlns="{NS}" xmlns:bst="urn:bevy-state-tree" bst:domain="test" datamodel="luau">
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
            r#"<scxml xmlns="{NS}" xmlns:bst="urn:bevy-state-tree" bst:domain="test" datamodel="luau">
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
            r#"<scxml xmlns="{NS}" xmlns:bst="urn:bevy-state-tree" bst:domain="test" datamodel="luau">
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
            r#"<scxml xmlns="{NS}" xmlns:bst="urn:bevy-state-tree" bst:domain="test" datamodel="luau">
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
            r#"<scxml xmlns="{NS}" xmlns:bst="urn:bevy-state-tree" bst:domain="test" datamodel="luau">
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
                r#"<scxml xmlns="{NS}" xmlns:bst="urn:bevy-state-tree" bst:domain="test" datamodel="luau"><state id="source"><transition cond="(" target="pass"/></state><final id="pass"/></scxml>"#
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
                r#"<scxml xmlns="{NS}" xmlns:bst="urn:bevy-state-tree" bst:domain="test" datamodel="luau"><datamodel><data id="value"/><data id="value"/></datamodel><state id="source"/></scxml>"#
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
                r#"<scxml xmlns="{NS}" xmlns:bst="urn:bevy-state-tree" bst:domain="test" datamodel="luau"><datamodel><data id="_event"/></datamodel><state id="source"/></scxml>"#
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

    struct CounterDomain;

    impl StateTreeDomain for CounterDomain {
        const AUTHORED_NAME: &'static str = "test";

        fn configure(config: &mut crate::StateTreeConfig<'_, Self>) {
            config.event::<CounterEvent>();
        }
    }

    #[derive(bevy_ecs::event::Event, crate::StateTreeEvent)]
    #[state_tree(domain = CounterDomain, name = "tick", broadcast)]
    struct CounterEvent;

    #[test]
    fn instances_share_bytecode_but_not_luau_data() {
        let document = scxml::parse(
            "counter.scxml",
            &format!(
                r#"<scxml xmlns="{NS}" xmlns:bst="urn:bevy-state-tree" bst:domain="test" datamodel="luau">
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
            TestApp::<CounterDomain>::new(definition.clone(), NonZeroUsize::new(8).unwrap());
        let second = TestApp::<CounterDomain>::new(definition, NonZeroUsize::new(8).unwrap());

        assert!(first.process_event(CounterEvent));

        assert_eq!(first.data("count"), Some(&LuauValue::Integer(1)));
        assert_eq!(second.data("count"), Some(&LuauValue::Integer(0)));

        assert!(first.process_event(CounterEvent));
        assert_eq!(first.data("count"), Some(&LuauValue::Integer(2)));
    }
}
