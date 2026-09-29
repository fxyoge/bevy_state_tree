use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use bevy_asset::Asset;
use bevy_reflect::TypePath;
use thiserror::Error;

use crate::effect::{RegisteredEffect, StateTreeEffectRegistry};
use crate::{StateTreeDomain, StateTreeEvents, luau::CompiledLuauExpression};

/// A compact state identifier local to one [`StateTreeDefinition`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StateId {
    definition: u64,
    index: u32,
}

impl StateId {
    pub(crate) fn index(self) -> usize {
        self.index as usize
    }
}

static NEXT_DEFINITION_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct EventId(u32);

impl EventId {
    fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DataId(u32);

impl DataId {
    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExpressionId(u32);

impl ExpressionId {
    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct EffectId(u32);

impl EffectId {
    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Action {
    Raise(EventId),
    Assign {
        data: DataId,
        expression: ExpressionId,
    },
    Effect {
        effect: EffectId,
        argument_start: u32,
        argument_len: u32,
        span_start: u32,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct EffectArgument {
    pub name: Box<str>,
    pub expression: ExpressionId,
}

#[derive(Clone, Debug)]
pub(crate) struct EventSet {
    words: Box<[u64]>,
}

impl EventSet {
    fn for_name(events: &[EventPattern], name: &str) -> Self {
        let mut words = vec![0; events.len().div_ceil(64)];
        for (index, pattern) in events.iter().enumerate() {
            if event_pattern_matches(pattern, name) {
                words[index / 64] |= 1 << (index % 64);
            }
        }
        Self {
            words: words.into_boxed_slice(),
        }
    }

    pub fn contains(&self, event: EventId) -> bool {
        let index = event.index();
        self.words
            .get(index / 64)
            .is_some_and(|word| word & (1 << (index % 64)) != 0)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum EventPattern {
    Exact(Box<str>),
    Descriptor(Box<str>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StateKind {
    Atomic,
    Compound {
        initial: StateId,
        completion: EventId,
    },
    Parallel {
        completion: EventId,
    },
    Final,
}

#[derive(Clone, Debug)]
pub(crate) struct StateNode {
    pub name: Box<str>,
    pub parent: Option<StateId>,
    pub depth: u32,
    pub document_order: u32,
    pub kind: StateKind,
    pub child_start: u32,
    pub child_len: u32,
    pub entry_actions: Box<[Action]>,
    pub exit_actions: Box<[Action]>,
    pub initial_actions: Box<[Action]>,
    pub transitions: Box<[Transition]>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Entry {
    pub state: StateId,
    pub take_initial: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Transition {
    pub event: Option<EventId>,
    pub condition: Option<ExpressionId>,
    pub target: Option<StateId>,
    pub domain: Option<StateId>,
    pub exit_depth: u32,
    pub entry_start: u32,
    pub entry_len: u32,
    pub action_start: u32,
    pub action_len: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EventRequirement {
    event: EventId,
    span_start: Option<u32>,
}

#[derive(Clone, Debug)]
pub(crate) struct CompiledLuauDataModel {
    pub names: Box<[Box<str>]>,
    pub initializers: Box<[Option<ExpressionId>]>,
    pub expressions: Box<[CompiledLuauExpression]>,
}

#[derive(Clone, Debug)]
pub(crate) struct CompiledDefinition {
    pub identity: u64,
    pub name: Box<str>,
    pub root: StateId,
    pub states: Box<[StateNode]>,
    pub children: Box<[StateId]>,
    pub transition_entries: Box<[Entry]>,
    pub transition_actions: Box<[Action]>,
    pub effect_arguments: Box<[EffectArgument]>,
    pub initial_entries: Box<[Entry]>,
    pub max_depth: usize,
    pub max_active: usize,
    pub has_parallel: bool,
    pub datamodel: Option<CompiledLuauDataModel>,
    events: Box<[EventPattern]>,
    event_requirements: Box<[EventRequirement]>,
    internal_events: Box<[EventId]>,
    effects: Box<[Box<str>]>,
}

impl CompiledDefinition {
    #[cfg(test)]
    pub fn event(&self, name: &str) -> Option<EventId> {
        self.events
            .iter()
            .position(
                |pattern| matches!(pattern, EventPattern::Exact(value) if value.as_ref() == name),
            )
            .and_then(|index| u32::try_from(index).ok())
            .map(EventId)
    }

    pub fn state(&self, id: StateId) -> &StateNode {
        debug_assert_eq!(id.definition, self.identity);
        &self.states[id.index()]
    }

    pub fn children(&self, id: StateId) -> &[StateId] {
        let state = self.state(id);
        let start = state.child_start as usize;
        &self.children[start..start + state.child_len as usize]
    }

    pub fn internal_event_matches(&self, candidate: EventId, emitted: EventId) -> bool {
        let EventPattern::Exact(emitted) = &self.events[emitted.index()] else {
            return false;
        };
        event_pattern_matches(&self.events[candidate.index()], emitted)
    }

    pub fn event_name(&self, event: EventId) -> Option<&str> {
        match &self.events[event.index()] {
            EventPattern::Exact(name) => Some(name),
            EventPattern::Descriptor(_) => None,
        }
    }

    pub fn effect_name(&self, effect: EffectId) -> &str {
        &self.effects[effect.index()]
    }

    pub fn effect_names(&self) -> impl ExactSizeIterator<Item = &str> {
        self.effects.iter().map(Box::as_ref)
    }
}

pub(crate) struct LinkedDefinition<D: StateTreeDomain> {
    pub compiled: Arc<CompiledDefinition>,
    external_events: Box<[EventSet]>,
    effects: Box<[Option<Arc<dyn RegisteredEffect<D>>>]>,
}

impl<D: StateTreeDomain> std::fmt::Debug for LinkedDefinition<D> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LinkedDefinition")
            .field("compiled", &self.compiled)
            .finish_non_exhaustive()
    }
}

impl<D: StateTreeDomain> LinkedDefinition<D> {
    pub fn new(
        compiled: Arc<CompiledDefinition>,
        effects: Option<&StateTreeEffectRegistry<D>>,
    ) -> Result<Self, StateTreeLinkError> {
        let mut issues = Vec::new();
        for requirement in &compiled.event_requirements {
            let pattern = &compiled.events[requirement.event.index()];
            let matches_external = D::Event::AUTHORED_NAMES
                .iter()
                .any(|name| event_pattern_matches(pattern, name));
            let matches_internal = compiled.internal_events.iter().any(|event| {
                let EventPattern::Exact(name) = &compiled.events[event.index()] else {
                    return false;
                };
                event_pattern_matches(pattern, name)
            });
            let wildcard =
                matches!(pattern, EventPattern::Descriptor(value) if value.as_ref() == "*");
            if !matches_external && !matches_internal && !wildcard {
                issues.push(StateTreeLinkIssue::UnmatchedEventDescriptor {
                    descriptor: match pattern {
                        EventPattern::Exact(value) | EventPattern::Descriptor(value) => {
                            value.clone()
                        }
                    },
                    span_start: requirement.span_start.map(|span| span as usize),
                });
            }
        }
        if let Some(registry) = effects {
            for (index, name) in compiled.effects.iter().enumerate() {
                if registry.get(name).is_none() {
                    issues.push(StateTreeLinkIssue::MissingEffect {
                        effect: name.clone(),
                        span_start: first_effect_span(&compiled, EffectId(index as u32)),
                    });
                }
            }
        }
        if !issues.is_empty() {
            return Err(StateTreeLinkError {
                definition: compiled.name.clone(),
                issues: issues.into_boxed_slice(),
            });
        }
        let external_events = D::Event::AUTHORED_NAMES
            .iter()
            .map(|name| EventSet::for_name(&compiled.events, name))
            .collect();
        let linked_effects = compiled
            .effect_names()
            .map(|name| effects.and_then(|effects| effects.get(name)))
            .collect();
        Ok(Self {
            compiled,
            external_events,
            effects: linked_effects,
        })
    }

    pub fn external_event(&self, index: usize) -> Option<&EventSet> {
        self.external_events.get(index)
    }

    pub fn effect(&self, effect: EffectId) -> Option<&Arc<dyn RegisteredEffect<D>>> {
        self.effects.get(effect.index()).and_then(Option::as_ref)
    }
}

fn first_effect_span(compiled: &CompiledDefinition, effect: EffectId) -> Option<usize> {
    compiled
        .states
        .iter()
        .flat_map(|state| {
            state
                .entry_actions
                .iter()
                .chain(state.exit_actions.iter())
                .chain(state.initial_actions.iter())
        })
        .chain(compiled.transition_actions.iter())
        .find_map(|action| match action {
            Action::Effect {
                effect: candidate,
                span_start,
                ..
            } if *candidate == effect => Some(*span_start as usize),
            _ => None,
        })
}

/// A definition could not be linked to a domain's typed runtime protocol.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("state-tree definition `{definition}` has {count} link issue(s)", count = .issues.len())]
pub struct StateTreeLinkError {
    definition: Box<str>,
    issues: Box<[StateTreeLinkIssue]>,
}

impl StateTreeLinkError {
    /// Returns the authored definition name.
    pub fn definition(&self) -> &str {
        &self.definition
    }

    /// Returns every issue found during the same link attempt.
    pub fn issues(&self) -> &[StateTreeLinkIssue] {
        &self.issues
    }
}

/// One domain-dependent state-tree link problem.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum StateTreeLinkIssue {
    #[error("event descriptor `{descriptor}` matches no typed or internal event")]
    UnmatchedEventDescriptor {
        descriptor: Box<str>,
        span_start: Option<usize>,
    },
    #[error("effect `{effect}` is not registered for this domain")]
    MissingEffect {
        effect: Box<str>,
        span_start: Option<usize>,
    },
}

fn event_pattern_matches(pattern: &EventPattern, event: &str) -> bool {
    match pattern {
        EventPattern::Exact(name) => name.as_ref() == event,
        EventPattern::Descriptor(descriptor) => {
            let descriptor = descriptor.strip_suffix(".*").unwrap_or(descriptor);
            descriptor == "*"
                || descriptor == event
                || event
                    .strip_prefix(descriptor)
                    .is_some_and(|suffix| suffix.starts_with('.'))
        }
    }
}

/// An immutable, validated statechart definition.
///
/// Definitions are cheap to clone and suitable for storage in
/// `Assets<StateTreeDefinition>`.
#[derive(Asset, Clone, Debug, TypePath)]
pub struct StateTreeDefinition {
    pub(crate) compiled: Arc<CompiledDefinition>,
}

impl StateTreeDefinition {
    /// Returns the authored definition name.
    pub fn name(&self) -> &str {
        &self.compiled.name
    }

    /// Returns the state name associated with an ID from this definition.
    pub fn state_name(&self, state: StateId) -> Option<&str> {
        if state.definition != self.compiled.identity {
            return None;
        }
        self.compiled
            .states
            .get(state.index())
            .map(|node| node.name.as_ref())
    }

    /// Returns the number of states in the definition.
    pub fn state_count(&self) -> usize {
        self.compiled.states.len()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DraftKind {
    Atomic,
    Compound { initial: Option<StateId> },
    Parallel,
    Final,
}

#[derive(Clone, Debug)]
struct DraftState {
    name: Box<str>,
    parent: Option<StateId>,
    kind: DraftKind,
    entry_actions: Vec<DraftAction>,
    exit_actions: Vec<DraftAction>,
    initial_actions: Vec<DraftAction>,
    transitions: Vec<DraftTransition>,
}

#[derive(Clone, Debug)]
struct DraftTransition {
    event: Option<EventPattern>,
    condition: Option<ExpressionId>,
    target: Option<StateId>,
    actions: Vec<DraftAction>,
    span_start: Option<u32>,
}

#[derive(Clone, Debug)]
pub(crate) enum DraftAction {
    Raise(Box<str>),
    Assign {
        data: DataId,
        expression: ExpressionId,
    },
    Effect {
        name: Box<str>,
        arguments: Vec<(Box<str>, ExpressionId)>,
        span_start: usize,
    },
}

#[derive(Clone, Debug)]
struct DraftLuauDataModel {
    names: Vec<Box<str>>,
    initializers: Vec<Option<ExpressionId>>,
    expressions: Vec<CompiledLuauExpression>,
}

/// Builds and validates a [`StateTreeDefinition`].
///
/// Parsers such as an SCXML loader should target this builder rather than
/// constructing runtime state directly.
#[derive(Debug)]
pub struct StateTreeBuilder {
    identity: u64,
    name: Box<str>,
    states: Vec<DraftState>,
    names: HashMap<Box<str>, StateId>,
    root: Option<StateId>,
    datamodel: Option<DraftLuauDataModel>,
}

impl StateTreeBuilder {
    /// Starts a definition with an authored diagnostic name.
    pub fn new(name: impl Into<Box<str>>) -> Self {
        Self {
            identity: NEXT_DEFINITION_ID.fetch_add(1, Ordering::Relaxed),
            name: name.into(),
            states: Vec::new(),
            names: HashMap::new(),
            root: None,
            datamodel: None,
        }
    }

    /// Adds the single compound root state.
    pub fn compound_root(&mut self, name: impl Into<Box<str>>) -> Result<StateId, BuildError> {
        if self.root.is_some() {
            return Err(BuildError::MultipleRoots);
        }

        let id = self.add_state(name, None, DraftKind::Compound { initial: None })?;
        self.root = Some(id);
        Ok(id)
    }

    /// Adds a compound child state.
    pub fn compound(
        &mut self,
        name: impl Into<Box<str>>,
        parent: StateId,
    ) -> Result<StateId, BuildError> {
        self.add_child(name, parent, DraftKind::Compound { initial: None })
    }

    /// Adds a parallel child state whose direct children are orthogonal regions.
    pub fn parallel(
        &mut self,
        name: impl Into<Box<str>>,
        parent: StateId,
    ) -> Result<StateId, BuildError> {
        self.add_child(name, parent, DraftKind::Parallel)
    }

    /// Adds an atomic child state.
    pub fn atomic(
        &mut self,
        name: impl Into<Box<str>>,
        parent: StateId,
    ) -> Result<StateId, BuildError> {
        self.add_child(name, parent, DraftKind::Atomic)
    }

    /// Adds a final child state.
    ///
    /// Entering a final child of a nested compound state raises the internal
    /// event `done.state.<parent-name>`. Entering a final child of the root
    /// completes the instance.
    pub fn final_state(
        &mut self,
        name: impl Into<Box<str>>,
        parent: StateId,
    ) -> Result<StateId, BuildError> {
        self.add_child(name, parent, DraftKind::Final)
    }

    /// Selects the direct child entered with a compound state.
    pub fn initial(&mut self, compound: StateId, child: StateId) -> Result<&mut Self, BuildError> {
        self.require_state(child)?;
        if self.states[child.index()].parent != Some(compound) {
            return Err(BuildError::InitialNotDirectChild { compound, child });
        }

        let state = self.require_state_mut(compound)?;
        let DraftKind::Compound { initial } = &mut state.kind else {
            return Err(BuildError::InitialOnNonCompound(compound));
        };
        *initial = Some(child);
        Ok(self)
    }

    /// Adds an external transition selected by an authored event name.
    pub fn transition(
        &mut self,
        source: StateId,
        event: impl Into<Box<str>>,
        target: StateId,
    ) -> Result<&mut Self, BuildError> {
        let event = event.into();
        if event.trim().is_empty() {
            return Err(BuildError::EmptyEventName);
        }
        self.add_transition(
            source,
            Some(EventPattern::Exact(event)),
            None,
            Some(target),
            Vec::new(),
            None,
        )
    }

    pub(crate) fn event_descriptor_with_actions(
        &mut self,
        source: StateId,
        descriptor: impl Into<Box<str>>,
        condition: Option<ExpressionId>,
        target: StateId,
        actions: Vec<DraftAction>,
        span_start: Option<usize>,
    ) -> Result<&mut Self, BuildError> {
        let descriptor = descriptor.into();
        if descriptor.trim().is_empty() {
            return Err(BuildError::EmptyEventName);
        }
        self.add_transition(
            source,
            Some(EventPattern::Descriptor(descriptor)),
            condition,
            Some(target),
            actions,
            span_start.and_then(|span| u32::try_from(span).ok()),
        )
    }

    #[cfg(test)]
    pub(crate) fn event_descriptor(
        &mut self,
        source: StateId,
        descriptor: impl Into<Box<str>>,
        target: StateId,
    ) -> Result<&mut Self, BuildError> {
        self.event_descriptor_with_actions(source, descriptor, None, target, Vec::new(), None)
    }

    /// Adds an eventless external transition.
    pub fn eventless(&mut self, source: StateId, target: StateId) -> Result<&mut Self, BuildError> {
        self.add_transition(source, None, None, Some(target), Vec::new(), None)
    }

    pub(crate) fn eventless_with_actions(
        &mut self,
        source: StateId,
        condition: Option<ExpressionId>,
        target: StateId,
        actions: Vec<DraftAction>,
        span_start: Option<usize>,
    ) -> Result<&mut Self, BuildError> {
        self.add_transition(
            source,
            None,
            condition,
            Some(target),
            actions,
            span_start.and_then(|span| u32::try_from(span).ok()),
        )
    }

    pub(crate) fn targetless_with_actions(
        &mut self,
        source: StateId,
        event: Option<Box<str>>,
        condition: Option<ExpressionId>,
        actions: Vec<DraftAction>,
        span_start: Option<usize>,
    ) -> Result<&mut Self, BuildError> {
        let event = event.map(EventPattern::Descriptor);
        self.add_transition(
            source,
            event,
            condition,
            None,
            actions,
            span_start.and_then(|span| u32::try_from(span).ok()),
        )
    }

    pub(crate) fn entry_actions(
        &mut self,
        state: StateId,
        actions: Vec<DraftAction>,
    ) -> Result<&mut Self, BuildError> {
        self.require_state_mut(state)?.entry_actions = actions;
        Ok(self)
    }

    pub(crate) fn exit_actions(
        &mut self,
        state: StateId,
        actions: Vec<DraftAction>,
    ) -> Result<&mut Self, BuildError> {
        self.require_state_mut(state)?.exit_actions = actions;
        Ok(self)
    }

    pub(crate) fn initial_actions(
        &mut self,
        state: StateId,
        actions: Vec<DraftAction>,
    ) -> Result<&mut Self, BuildError> {
        self.require_state_mut(state)?.initial_actions = actions;
        Ok(self)
    }

    pub(crate) fn set_luau_datamodel(
        &mut self,
        names: Vec<Box<str>>,
        initializers: Vec<Option<CompiledLuauExpression>>,
    ) -> Result<Vec<DataId>, BuildError> {
        if self.datamodel.is_some() {
            return Err(BuildError::MultipleDataModels);
        }
        let mut expressions = Vec::new();
        let mut initializer_ids = Vec::with_capacity(initializers.len());
        for initializer in initializers {
            initializer_ids.push(match initializer {
                Some(expression) => {
                    let id = ExpressionId(
                        u32::try_from(expressions.len())
                            .map_err(|_| BuildError::TooManyExpressions)?,
                    );
                    expressions.push(expression);
                    Some(id)
                }
                None => None,
            });
        }
        let ids = (0..names.len())
            .map(|index| {
                u32::try_from(index)
                    .map(DataId)
                    .map_err(|_| BuildError::TooManyDataItems)
            })
            .collect::<Result<_, _>>()?;
        self.datamodel = Some(DraftLuauDataModel {
            names,
            initializers: initializer_ids,
            expressions,
        });
        Ok(ids)
    }

    pub(crate) fn luau_expression(
        &mut self,
        expression: CompiledLuauExpression,
    ) -> Result<ExpressionId, BuildError> {
        let datamodel = self
            .datamodel
            .as_mut()
            .ok_or(BuildError::MissingDataModel)?;
        let id = ExpressionId(
            u32::try_from(datamodel.expressions.len())
                .map_err(|_| BuildError::TooManyExpressions)?,
        );
        datamodel.expressions.push(expression);
        Ok(id)
    }

    /// Validates and compiles the definition.
    pub fn build(self) -> Result<StateTreeDefinition, BuildError> {
        if self.name.is_empty() {
            return Err(BuildError::EmptyDefinitionName);
        }
        let root = self.root.ok_or(BuildError::MissingRoot)?;

        let mut depths = Vec::with_capacity(self.states.len());
        for draft in &self.states {
            let depth = match draft.parent {
                Some(parent) => depths[parent.index()] + 1,
                None => 0,
            };
            depths.push(depth);
        }

        let mut events = Vec::<EventPattern>::new();
        let mut event_ids = HashMap::<EventPattern, EventId>::new();
        let mut event_requirements = Vec::new();
        let mut states = Vec::with_capacity(self.states.len());
        let mut transition_entries = Vec::new();
        let mut transition_actions = Vec::new();
        let mut effects = Vec::<Box<str>>::new();
        let mut effect_ids = HashMap::<Box<str>, EffectId>::new();
        let mut effect_arguments = Vec::new();
        let mut children_by_state = vec![Vec::new(); self.states.len()];
        for (index, draft) in self.states.iter().enumerate() {
            if let Some(parent) = draft.parent {
                children_by_state[parent.index()].push(StateId {
                    definition: self.identity,
                    index: u32::try_from(index).map_err(|_| BuildError::TooManyStates)?,
                });
            }
        }
        let mut children = Vec::with_capacity(self.states.len().saturating_sub(1));
        let has_parallel = self
            .states
            .iter()
            .any(|state| state.kind == DraftKind::Parallel);
        let mut document_orders = vec![0; self.states.len()];
        let mut order = 0u32;
        let mut pending = vec![root];
        while let Some(state) = pending.pop() {
            document_orders[state.index()] = order;
            order = order.checked_add(1).ok_or(BuildError::TooManyStates)?;
            pending.extend(children_by_state[state.index()].iter().rev().copied());
        }

        for (index, draft) in self.states.iter().enumerate() {
            let state_id = StateId {
                definition: self.identity,
                index: u32::try_from(index).map_err(|_| BuildError::TooManyStates)?,
            };
            let kind = match draft.kind {
                DraftKind::Atomic => StateKind::Atomic,
                DraftKind::Final => {
                    if !draft.transitions.is_empty() {
                        return Err(BuildError::FinalHasTransition(state_id));
                    }
                    StateKind::Final
                }
                DraftKind::Compound { initial } => {
                    let completion_name: Box<str> = format!("done.state.{}", draft.name).into();
                    let pattern = EventPattern::Exact(completion_name.clone());
                    let completion = intern_event(&mut events, &mut event_ids, pattern)?;
                    StateKind::Compound {
                        initial: initial.ok_or(BuildError::MissingInitial(state_id))?,
                        completion,
                    }
                }
                DraftKind::Parallel => {
                    if children_by_state[index].is_empty() {
                        return Err(BuildError::ParallelHasNoRegions(state_id));
                    }
                    let completion_name: Box<str> = format!("done.state.{}", draft.name).into();
                    let completion = intern_event(
                        &mut events,
                        &mut event_ids,
                        EventPattern::Exact(completion_name),
                    )?;
                    StateKind::Parallel { completion }
                }
            };

            let mut transitions = Vec::with_capacity(draft.transitions.len());
            for transition in &draft.transitions {
                let event = match &transition.event {
                    Some(pattern) => {
                        let event = intern_event(&mut events, &mut event_ids, pattern.clone())?;
                        Some(event)
                    }
                    None => None,
                };
                if let Some(event) = event {
                    event_requirements.push(EventRequirement {
                        event,
                        span_start: transition.span_start,
                    });
                }
                let (exit_depth, entry_start, entry_len) = if let Some(target) = transition.target {
                    let entry_start = u32::try_from(transition_entries.len())
                        .map_err(|_| BuildError::TooManyStates)?;
                    let entry_len = append_transition_entries(
                        state_id,
                        target,
                        &self.states,
                        &depths,
                        &children_by_state,
                        &mut transition_entries,
                    )?;
                    (
                        transition_exit_depth(state_id, target, &self.states, &depths),
                        entry_start,
                        entry_len,
                    )
                } else {
                    (0, 0, 0)
                };
                transitions.push(Transition {
                    event,
                    condition: transition.condition,
                    target: transition.target,
                    domain: transition.target.and_then(|target| {
                        transition_domain(state_id, target, &self.states, &depths)
                    }),
                    exit_depth,
                    entry_start,
                    entry_len,
                    action_start: u32::try_from(transition_actions.len())
                        .map_err(|_| BuildError::TooManyActions)?,
                    action_len: append_actions(
                        &transition.actions,
                        &mut events,
                        &mut event_ids,
                        &mut effects,
                        &mut effect_ids,
                        &mut effect_arguments,
                        &mut transition_actions,
                    )?,
                });
            }

            let child_start =
                u32::try_from(children.len()).map_err(|_| BuildError::TooManyStates)?;
            children.extend_from_slice(&children_by_state[index]);
            states.push(StateNode {
                name: draft.name.clone(),
                parent: draft.parent,
                depth: depths[index],
                document_order: document_orders[index],
                kind,
                child_start,
                child_len: u32::try_from(children_by_state[index].len())
                    .map_err(|_| BuildError::TooManyStates)?,
                entry_actions: compile_actions(
                    &draft.entry_actions,
                    &mut events,
                    &mut event_ids,
                    &mut effects,
                    &mut effect_ids,
                    &mut effect_arguments,
                )?,
                exit_actions: compile_actions(
                    &draft.exit_actions,
                    &mut events,
                    &mut event_ids,
                    &mut effects,
                    &mut effect_ids,
                    &mut effect_arguments,
                )?,
                initial_actions: compile_actions(
                    &draft.initial_actions,
                    &mut events,
                    &mut event_ids,
                    &mut effects,
                    &mut effect_ids,
                    &mut effect_arguments,
                )?,
                transitions: transitions.into_boxed_slice(),
            });
        }

        let mut initial_entries = Vec::new();
        append_entry_configuration(
            root,
            None,
            &self.states,
            &children_by_state,
            &mut initial_entries,
        );
        let max_depth = depths.iter().copied().max().unwrap_or(0).saturating_add(1) as usize;
        let mut max_configurations = vec![1usize; self.states.len()];
        for index in (0..self.states.len()).rev() {
            max_configurations[index] = match self.states[index].kind {
                DraftKind::Atomic | DraftKind::Final => 1,
                DraftKind::Compound { .. } => {
                    1 + children_by_state[index]
                        .iter()
                        .map(|child| max_configurations[child.index()])
                        .max()
                        .unwrap_or(0)
                }
                DraftKind::Parallel => {
                    1 + children_by_state[index]
                        .iter()
                        .map(|child| max_configurations[child.index()])
                        .sum::<usize>()
                }
            };
        }

        let datamodel = self.datamodel.map(|datamodel| CompiledLuauDataModel {
            names: datamodel.names.into_boxed_slice(),
            initializers: datamodel.initializers.into_boxed_slice(),
            expressions: datamodel.expressions.into_boxed_slice(),
        });
        let mut completable = vec![false; states.len()];
        for index in (0..states.len()).rev() {
            completable[index] = match states[index].kind {
                StateKind::Compound { .. } => {
                    let start = states[index].child_start as usize;
                    let end = start + states[index].child_len as usize;
                    children[start..end]
                        .iter()
                        .any(|child| states[child.index()].kind == StateKind::Final)
                }
                StateKind::Parallel { .. } => {
                    let start = states[index].child_start as usize;
                    let end = start + states[index].child_len as usize;
                    !children[start..end].is_empty()
                        && children[start..end]
                            .iter()
                            .all(|child| completable[child.index()])
                }
                StateKind::Atomic | StateKind::Final => false,
            };
        }
        let mut internal_events = Vec::new();
        for (index, state) in states.iter().enumerate() {
            if let StateKind::Compound { completion, .. } | StateKind::Parallel { completion } =
                state.kind
                && state.parent.is_some()
                && completable[index]
                && !internal_events.contains(&completion)
            {
                internal_events.push(completion);
            }
            for action in state
                .entry_actions
                .iter()
                .chain(state.exit_actions.iter())
                .chain(state.initial_actions.iter())
            {
                if let Action::Raise(event) = action
                    && !internal_events.contains(event)
                {
                    internal_events.push(*event);
                }
            }
        }
        for action in &transition_actions {
            if let Action::Raise(event) = action
                && !internal_events.contains(event)
            {
                internal_events.push(*event);
            }
        }

        Ok(StateTreeDefinition {
            compiled: Arc::new(CompiledDefinition {
                identity: self.identity,
                name: self.name,
                root,
                states: states.into_boxed_slice(),
                children: children.into_boxed_slice(),
                transition_entries: transition_entries.into_boxed_slice(),
                transition_actions: transition_actions.into_boxed_slice(),
                effect_arguments: effect_arguments.into_boxed_slice(),
                initial_entries: initial_entries.into_boxed_slice(),
                max_depth,
                max_active: max_configurations[root.index()],
                has_parallel,
                datamodel,
                events: events.into_boxed_slice(),
                event_requirements: event_requirements.into_boxed_slice(),
                internal_events: internal_events.into_boxed_slice(),
                effects: effects.into_boxed_slice(),
            }),
        })
    }

    fn add_child(
        &mut self,
        name: impl Into<Box<str>>,
        parent: StateId,
        kind: DraftKind,
    ) -> Result<StateId, BuildError> {
        let parent_state = self.require_state(parent)?;
        if !matches!(
            parent_state.kind,
            DraftKind::Compound { .. } | DraftKind::Parallel
        ) {
            return Err(BuildError::ChildOfNonCompound(parent));
        }
        if kind == DraftKind::Final && parent_state.kind == DraftKind::Parallel {
            return Err(BuildError::FinalChildOfParallel(parent));
        }
        self.add_state(name, Some(parent), kind)
    }

    fn add_state(
        &mut self,
        name: impl Into<Box<str>>,
        parent: Option<StateId>,
        kind: DraftKind,
    ) -> Result<StateId, BuildError> {
        let name = name.into();
        if name.is_empty() {
            return Err(BuildError::EmptyStateName);
        }
        if self.names.contains_key(name.as_ref()) {
            return Err(BuildError::DuplicateStateName(name));
        }
        let raw = u32::try_from(self.states.len()).map_err(|_| BuildError::TooManyStates)?;
        let id = StateId {
            definition: self.identity,
            index: raw,
        };
        self.states.push(DraftState {
            name: name.clone(),
            parent,
            kind,
            entry_actions: Vec::new(),
            exit_actions: Vec::new(),
            initial_actions: Vec::new(),
            transitions: Vec::new(),
        });
        self.names.insert(name, id);
        Ok(id)
    }

    fn add_transition(
        &mut self,
        source: StateId,
        event: Option<EventPattern>,
        condition: Option<ExpressionId>,
        target: Option<StateId>,
        actions: Vec<DraftAction>,
        span_start: Option<u32>,
    ) -> Result<&mut Self, BuildError> {
        if let Some(target) = target {
            self.require_state(target)?;
        }
        let state = self.require_state_mut(source)?;
        if matches!(state.kind, DraftKind::Final) {
            return Err(BuildError::FinalHasTransition(source));
        }
        state.transitions.push(DraftTransition {
            event,
            condition,
            target,
            actions,
            span_start,
        });
        Ok(self)
    }

    fn require_state(&self, id: StateId) -> Result<&DraftState, BuildError> {
        if id.definition != self.identity {
            return Err(BuildError::ForeignState(id));
        }
        self.states
            .get(id.index())
            .ok_or(BuildError::UnknownState(id))
    }

    fn require_state_mut(&mut self, id: StateId) -> Result<&mut DraftState, BuildError> {
        if id.definition != self.identity {
            return Err(BuildError::ForeignState(id));
        }
        self.states
            .get_mut(id.index())
            .ok_or(BuildError::UnknownState(id))
    }
}

fn intern_event(
    events: &mut Vec<EventPattern>,
    event_ids: &mut HashMap<EventPattern, EventId>,
    pattern: EventPattern,
) -> Result<EventId, BuildError> {
    if let Some(event) = event_ids.get(&pattern) {
        return Ok(*event);
    }
    let event = EventId(u32::try_from(events.len()).map_err(|_| BuildError::TooManyEvents)?);
    events.push(pattern.clone());
    event_ids.insert(pattern, event);
    Ok(event)
}

fn compile_actions(
    actions: &[DraftAction],
    events: &mut Vec<EventPattern>,
    event_ids: &mut HashMap<EventPattern, EventId>,
    effects: &mut Vec<Box<str>>,
    effect_ids: &mut HashMap<Box<str>, EffectId>,
    effect_arguments: &mut Vec<EffectArgument>,
) -> Result<Box<[Action]>, BuildError> {
    actions
        .iter()
        .map(|action| {
            compile_action(
                action,
                events,
                event_ids,
                effects,
                effect_ids,
                effect_arguments,
            )
        })
        .collect()
}

fn append_actions(
    actions: &[DraftAction],
    events: &mut Vec<EventPattern>,
    event_ids: &mut HashMap<EventPattern, EventId>,
    effects: &mut Vec<Box<str>>,
    effect_ids: &mut HashMap<Box<str>, EffectId>,
    effect_arguments: &mut Vec<EffectArgument>,
    compiled: &mut Vec<Action>,
) -> Result<u32, BuildError> {
    let start = compiled.len();
    for action in actions {
        compiled.push(compile_action(
            action,
            events,
            event_ids,
            effects,
            effect_ids,
            effect_arguments,
        )?);
    }
    u32::try_from(compiled.len() - start).map_err(|_| BuildError::TooManyActions)
}

fn compile_action(
    action: &DraftAction,
    events: &mut Vec<EventPattern>,
    event_ids: &mut HashMap<EventPattern, EventId>,
    effects: &mut Vec<Box<str>>,
    effect_ids: &mut HashMap<Box<str>, EffectId>,
    effect_arguments: &mut Vec<EffectArgument>,
) -> Result<Action, BuildError> {
    match action {
        DraftAction::Raise(event) => {
            intern_event(events, event_ids, EventPattern::Exact(event.clone())).map(Action::Raise)
        }
        DraftAction::Assign { data, expression } => Ok(Action::Assign {
            data: *data,
            expression: *expression,
        }),
        DraftAction::Effect {
            name,
            arguments,
            span_start,
        } => {
            let effect = if let Some(effect) = effect_ids.get(name) {
                *effect
            } else {
                let effect =
                    EffectId(u32::try_from(effects.len()).map_err(|_| BuildError::TooManyEffects)?);
                effects.push(name.clone());
                effect_ids.insert(name.clone(), effect);
                effect
            };
            let argument_start = u32::try_from(effect_arguments.len())
                .map_err(|_| BuildError::TooManyEffectArguments)?;
            effect_arguments.extend(arguments.iter().map(|(name, expression)| EffectArgument {
                name: name.clone(),
                expression: *expression,
            }));
            Ok(Action::Effect {
                effect,
                argument_start,
                argument_len: u32::try_from(arguments.len())
                    .map_err(|_| BuildError::TooManyEffectArguments)?,
                span_start: u32::try_from(*span_start).unwrap_or(u32::MAX),
            })
        }
    }
}

fn transition_domain(
    source: StateId,
    target: StateId,
    states: &[DraftState],
    depths: &[u32],
) -> Option<StateId> {
    let mut source_ancestor = Some(source);
    let mut source_depth = depths[source.index()];
    let mut target_ancestor = Some(target);
    let mut target_depth = depths[target.index()];

    while source_depth > target_depth {
        source_ancestor = source_ancestor.and_then(|state| states[state.index()].parent);
        source_depth -= 1;
    }
    while target_depth > source_depth {
        target_ancestor = target_ancestor.and_then(|state| states[state.index()].parent);
        target_depth -= 1;
    }
    while source_ancestor != target_ancestor {
        source_ancestor = source_ancestor.and_then(|state| states[state.index()].parent);
        target_ancestor = target_ancestor.and_then(|state| states[state.index()].parent);
    }

    // External transitions exit and re-enter their source when it is the LCA.
    if source_ancestor == Some(source) || source_ancestor == Some(target) {
        source_ancestor.and_then(|state| states[state.index()].parent)
    } else {
        source_ancestor
    }
}

fn transition_exit_depth(
    source: StateId,
    target: StateId,
    states: &[DraftState],
    depths: &[u32],
) -> u32 {
    transition_domain(source, target, states, depths).map_or(0, |state| depths[state.index()] + 1)
}

fn append_transition_entries(
    source: StateId,
    target: StateId,
    states: &[DraftState],
    depths: &[u32],
    children: &[Vec<StateId>],
    entries: &mut Vec<Entry>,
) -> Result<u32, BuildError> {
    let domain = transition_domain(source, target, states, depths);
    let start = entries.len();
    let mut reverse = Vec::with_capacity(depths[target.index()] as usize + 1);
    let mut cursor = Some(target);
    while cursor != domain {
        let state = cursor.expect("transition target and source share the definition root");
        reverse.push(state);
        cursor = states[state.index()].parent;
    }
    reverse.reverse();

    if domain.is_some_and(|state| states[state.index()].kind == DraftKind::Parallel) {
        let explicit = reverse.first().copied();
        for child in &children[domain.expect("checked domain").index()] {
            let path = (Some(*child) == explicit).then_some(reverse.as_slice());
            append_entry_configuration(*child, path, states, children, entries);
        }
    } else {
        let first = reverse
            .first()
            .copied()
            .expect("an external transition always re-enters at least its target");
        append_entry_configuration(first, Some(&reverse), states, children, entries);
    }

    u32::try_from(entries.len() - start).map_err(|_| BuildError::TooManyStates)
}

fn append_entry_configuration(
    state: StateId,
    explicit_path: Option<&[StateId]>,
    states: &[DraftState],
    children: &[Vec<StateId>],
    entries: &mut Vec<Entry>,
) {
    #[derive(Clone, Copy)]
    struct Pending<'a> {
        state: StateId,
        explicit_path: Option<&'a [StateId]>,
    }

    let mut pending = vec![Pending {
        state,
        explicit_path,
    }];
    while let Some(Pending {
        state,
        explicit_path,
    }) = pending.pop()
    {
        let explicit_child = explicit_path.and_then(|path| path.get(1)).copied();
        let take_initial = matches!(states[state.index()].kind, DraftKind::Compound { .. })
            && explicit_child.is_none();
        entries.push(Entry {
            state,
            take_initial,
        });

        match states[state.index()].kind {
            DraftKind::Atomic | DraftKind::Final => {}
            DraftKind::Compound { initial } => {
                let child = explicit_child.or(initial);
                if let Some(child) = child {
                    pending.push(Pending {
                        state: child,
                        explicit_path: explicit_child
                            .is_some()
                            .then(|| &explicit_path.expect("explicit child requires a path")[1..]),
                    });
                }
            }
            DraftKind::Parallel => {
                for child in children[state.index()].iter().rev() {
                    pending.push(Pending {
                        state: *child,
                        explicit_path: (Some(*child) == explicit_child)
                            .then(|| &explicit_path.expect("explicit child requires a path")[1..]),
                    });
                }
            }
        }
    }
}

/// A state-tree definition validation error.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum BuildError {
    #[error("the definition name cannot be empty")]
    EmptyDefinitionName,
    #[error("a state name cannot be empty")]
    EmptyStateName,
    #[error("an event name cannot be empty")]
    EmptyEventName,
    #[error("state name `{0}` is duplicated")]
    DuplicateStateName(Box<str>),
    #[error("a definition must have exactly one compound root")]
    MissingRoot,
    #[error("a definition cannot have more than one root")]
    MultipleRoots,
    #[error("state {0:?} does not exist")]
    UnknownState(StateId),
    #[error("state {0:?} belongs to another definition")]
    ForeignState(StateId),
    #[error("state {0:?} cannot contain child states")]
    ChildOfNonCompound(StateId),
    #[error("state {0:?} is not compound and cannot have an initial child")]
    InitialOnNonCompound(StateId),
    #[error("state {child:?} is not a direct child of {compound:?}")]
    InitialNotDirectChild { compound: StateId, child: StateId },
    #[error("compound state {0:?} has no initial child")]
    MissingInitial(StateId),
    #[error("parallel state {0:?} has no regions")]
    ParallelHasNoRegions(StateId),
    #[error("final state {0:?} cannot have outgoing transitions")]
    FinalHasTransition(StateId),
    #[error("parallel state {0:?} cannot contain a final child")]
    FinalChildOfParallel(StateId),
    #[error("a definition cannot contain more than u32::MAX states")]
    TooManyStates,
    #[error("a definition cannot contain more than u32::MAX event names")]
    TooManyEvents,
    #[error("a definition cannot contain more than u32::MAX executable actions")]
    TooManyActions,
    #[error("a definition cannot contain more than one datamodel")]
    MultipleDataModels,
    #[error("a Luau expression requires a datamodel")]
    MissingDataModel,
    #[error("a definition cannot contain more than u32::MAX data items")]
    TooManyDataItems,
    #[error("a definition cannot contain more than u32::MAX expressions")]
    TooManyExpressions,
    #[error("a definition cannot contain more than u32::MAX effects")]
    TooManyEffects,
    #[error("a definition cannot contain more than u32::MAX effect arguments")]
    TooManyEffectArguments,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_deep_hierarchy_without_recursion() {
        let mut builder = StateTreeBuilder::new("deep");
        let root = builder.compound_root("root").unwrap();
        let mut parent = root;

        for depth in 0..10_000 {
            let child = builder.compound(format!("level-{depth}"), parent).unwrap();
            builder.initial(parent, child).unwrap();
            parent = child;
        }

        let leaf = builder.atomic("leaf", parent).unwrap();
        builder.initial(parent, leaf).unwrap();
        let definition = builder.build().unwrap();

        assert_eq!(definition.state_count(), 10_002);
        assert_eq!(definition.state_name(leaf), Some("leaf"));
    }

    #[test]
    fn preserves_ordered_transition_candidates() {
        let mut builder = StateTreeBuilder::new("candidates");
        let root = builder.compound_root("root").unwrap();
        let first = builder.atomic("first", root).unwrap();
        let second = builder.atomic("second", root).unwrap();
        builder.initial(root, first).unwrap();
        builder.transition(first, "advance", second).unwrap();
        builder.transition(first, "advance", first).unwrap();

        let definition = builder.build().unwrap();
        assert_eq!(
            definition.compiled.state(first).transitions[0].target,
            Some(second)
        );
        assert_eq!(
            definition.compiled.state(first).transitions[1].target,
            Some(first)
        );
    }

    #[test]
    fn rejects_invalid_hierarchy_and_incomplete_compounds() {
        let mut builder = StateTreeBuilder::new("invalid");
        let root = builder.compound_root("root").unwrap();
        let atomic = builder.atomic("atomic", root).unwrap();

        assert_eq!(
            builder.atomic("child", atomic),
            Err(BuildError::ChildOfNonCompound(atomic))
        );
        assert_eq!(
            builder.build().unwrap_err(),
            BuildError::MissingInitial(root)
        );
    }

    #[test]
    fn rejects_parallel_states_without_regions() {
        let mut builder = StateTreeBuilder::new("invalid-parallel");
        let root = builder.compound_root("root").unwrap();
        let parallel = builder.parallel("parallel", root).unwrap();
        builder.initial(root, parallel).unwrap();

        assert_eq!(
            builder.build().unwrap_err(),
            BuildError::ParallelHasNoRegions(parallel)
        );
    }

    #[test]
    fn rejects_final_states_directly_below_parallel_states() {
        let mut builder = StateTreeBuilder::new("invalid-parallel-final");
        let root = builder.compound_root("root").unwrap();
        let parallel = builder.parallel("parallel", root).unwrap();

        assert_eq!(
            builder.final_state("done", parallel),
            Err(BuildError::FinalChildOfParallel(parallel))
        );
    }

    #[test]
    fn rejects_whitespace_only_event_names() {
        let mut builder = StateTreeBuilder::new("invalid-event");
        let root = builder.compound_root("root").unwrap();
        let source = builder.atomic("source", root).unwrap();
        let target = builder.atomic("target", root).unwrap();

        assert_eq!(
            builder.transition(source, "  ", target).unwrap_err(),
            BuildError::EmptyEventName
        );
    }

    #[test]
    fn rejects_state_ids_from_another_builder() {
        let mut first = StateTreeBuilder::new("first");
        let foreign_root = first.compound_root("root").unwrap();

        let mut second = StateTreeBuilder::new("second");
        let root = second.compound_root("root").unwrap();

        assert_eq!(
            second.atomic("invalid", foreign_root),
            Err(BuildError::ForeignState(foreign_root))
        );
        let leaf = second.atomic("leaf", root).unwrap();
        second.initial(root, leaf).unwrap();
        let definition = second.build().unwrap();
        assert_eq!(definition.state_name(foreign_root), None);
    }
}
