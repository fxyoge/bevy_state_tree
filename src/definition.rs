use std::{
    collections::{HashMap, HashSet},
    num::NonZeroU32,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use bevy_asset::Asset;
use bevy_reflect::TypePath;
use thiserror::Error;

use crate::activity::{RegisteredActivity, StateTreeActivityRegistry};
use crate::effect::{RegisteredEffect, StateTreeEffectRegistry};
use crate::event::StateTreeEventProtocol;
use crate::{
    StateTreeDomain,
    luau::{CompiledLuauExpression, compile_expression},
};

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

/// A compact shallow-history identifier local to one [`StateTreeDefinition`].
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HistoryId {
    definition: u64,
    index: u32,
}

impl HistoryId {
    pub(crate) fn index(self) -> usize {
        self.index as usize
    }
}

/// A state-tree transition or compound initial target.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum StateTreeTarget {
    State(StateId),
    History(HistoryId),
}

impl From<StateId> for StateTreeTarget {
    fn from(state: StateId) -> Self {
        Self::State(state)
    }
}

impl From<HistoryId> for StateTreeTarget {
    fn from(history: HistoryId) -> Self {
        Self::History(history)
    }
}

impl PartialEq<StateId> for StateTreeTarget {
    fn eq(&self, other: &StateId) -> bool {
        *self == Self::State(*other)
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

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct TimerId(pub(crate) u32);

impl TimerId {
    pub(crate) fn index(self) -> usize {
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

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct ActivityId(pub(crate) u32);

impl ActivityId {
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
pub(crate) struct NamedArgument {
    pub name: Arc<str>,
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
            if event_pattern_name(pattern) == name {
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

    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|word| *word == 0)
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
        initial: StateTreeTarget,
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
    pub history_slot: Option<HistorySlotId>,
    pub activity_start: u32,
    pub activity_len: u32,
    pub transitions: Box<[Transition]>,
}

#[derive(Clone, Debug)]
pub(crate) struct ActivityDefinition {
    pub id: Box<str>,
    pub name: Box<str>,
    pub owner: StateId,
    pub argument_start: u32,
    pub argument_len: u32,
    pub done: EventId,
    pub error: EventId,
    pub span_start: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Entry {
    pub state: StateId,
    pub take_initial: bool,
    pub history_default: Option<HistoryIndex>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HistoryIndex(NonZeroU32);

impl HistoryIndex {
    pub(crate) fn from_id(id: HistoryId) -> Self {
        Self(NonZeroU32::new(id.index + 1).expect("history indices never equal u32::MAX"))
    }

    pub(crate) fn index(self) -> usize {
        (self.0.get() - 1) as usize
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HistorySlotId(u32);

impl HistorySlotId {
    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, Debug)]
pub(crate) struct HistoryDefinition {
    pub name: Box<str>,
    pub owner: StateId,
    pub slot: HistorySlotId,
    pub default: StateId,
    pub action_start: u32,
    pub action_len: u32,
    pub entry_start: u32,
    pub entry_len: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HistorySlotDefinition {
    pub owner: StateId,
    pub child_plan_start: u32,
    pub child_plan_len: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HistoryChildPlan {
    pub child: StateId,
    pub entry_start: u32,
    pub entry_len: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TransitionTrigger {
    Event(EventId),
    Eventless,
    After(TimerId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Transition {
    pub trigger: TransitionTrigger,
    pub condition: Option<ExpressionId>,
    pub target: Option<StateTreeTarget>,
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
    pub domain: Option<Box<str>>,
    pub domain_span_start: Option<usize>,
    pub root: StateId,
    pub states: Box<[StateNode]>,
    pub children: Box<[StateId]>,
    pub transition_entries: Box<[Entry]>,
    pub transition_actions: Box<[Action]>,
    pub named_arguments: Box<[NamedArgument]>,
    pub activities: Box<[ActivityDefinition]>,
    pub histories: Box<[HistoryDefinition]>,
    pub history_slots: Box<[HistorySlotDefinition]>,
    pub history_child_plans: Box<[HistoryChildPlan]>,
    pub history_entries: Box<[Entry]>,
    pub history_subtree_ends: Option<Box<[u32]>>,
    pub initial_entries: Box<[Entry]>,
    pub max_depth: usize,
    pub max_active: usize,
    pub has_parallel: bool,
    pub datamodel: Option<CompiledLuauDataModel>,
    pub timers: Box<[Duration]>,
    events: Box<[EventPattern]>,
    event_requirements: Box<[EventRequirement]>,
    internal_events: Box<[EventId]>,
    effects: Box<[Box<str>]>,
}

impl CompiledDefinition {
    pub fn state(&self, target: impl Into<StateTreeTarget>) -> &StateNode {
        let id = match target.into() {
            StateTreeTarget::State(state) => state,
            // Runtime tracing can still name an unresolved history target by its owner.
            StateTreeTarget::History(history) => self.history(history).owner,
        };
        debug_assert_eq!(id.definition, self.identity);
        &self.states[id.index()]
    }

    pub fn children(&self, id: StateId) -> &[StateId] {
        let state = self.state(id);
        let start = state.child_start as usize;
        &self.children[start..start + state.child_len as usize]
    }

    pub(crate) fn history(&self, id: HistoryId) -> &HistoryDefinition {
        debug_assert_eq!(id.definition, self.identity);
        &self.histories[id.index()]
    }

    pub(crate) fn history_by_index(&self, id: HistoryIndex) -> &HistoryDefinition {
        &self.histories[id.index()]
    }

    pub(crate) fn history_slot(&self, id: HistorySlotId) -> &HistorySlotDefinition {
        &self.history_slots[id.index()]
    }

    pub(crate) fn history_child_plans(&self, slot: HistorySlotId) -> &[HistoryChildPlan] {
        let slot = self.history_slot(slot);
        let start = slot.child_plan_start as usize;
        &self.history_child_plans[start..start + slot.child_plan_len as usize]
    }

    pub(crate) fn history_entries(&self, start: u32, len: u32) -> &[Entry] {
        let start = start as usize;
        &self.history_entries[start..start + len as usize]
    }

    pub fn internal_event_matches(&self, candidate: EventId, emitted: EventId) -> bool {
        let EventPattern::Exact(emitted) = &self.events[emitted.index()] else {
            return false;
        };
        event_pattern_name(&self.events[candidate.index()]) == emitted.as_ref()
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
    effects: Box<[Arc<dyn RegisteredEffect<D>>]>,
    activities: Box<[Arc<dyn RegisteredActivity<D>>]>,
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
        events: &StateTreeEventProtocol<D>,
        effects: &StateTreeEffectRegistry<D>,
        activities: &StateTreeActivityRegistry<D>,
    ) -> Result<Self, StateTreeLinkError> {
        if let Some(declared) = compiled.domain.as_deref()
            && declared != D::AUTHORED_NAME
        {
            return Err(StateTreeLinkError {
                definition: compiled.name.clone(),
                issues: Box::new([StateTreeLinkIssue::DomainMismatch {
                    declared: declared.into(),
                    expected: D::AUTHORED_NAME.into(),
                    span_start: compiled.domain_span_start,
                }]),
            });
        }
        let mut issues = Vec::new();
        for requirement in &compiled.event_requirements {
            let pattern = &compiled.events[requirement.event.index()];
            let matches_external = events
                .authored_names()
                .iter()
                .any(|name| event_pattern_name(pattern) == *name);
            let matches_internal = compiled.internal_events.iter().any(|event| {
                let EventPattern::Exact(name) = &compiled.events[event.index()] else {
                    return false;
                };
                event_pattern_name(pattern) == name.as_ref()
            });
            if !matches_external && !matches_internal {
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
        for (index, name) in compiled.effects.iter().enumerate() {
            if effects.get(name).is_none() {
                issues.push(StateTreeLinkIssue::MissingEffect {
                    effect: name.clone(),
                    span_start: first_effect_span(&compiled, EffectId(index as u32)),
                });
            }
        }
        for activity in &compiled.activities {
            if activities.get(&activity.name).is_none() {
                issues.push(StateTreeLinkIssue::MissingActivity {
                    activity: activity.name.clone(),
                    span_start: Some(activity.span_start as usize),
                });
            }
        }
        if !issues.is_empty() {
            return Err(StateTreeLinkError {
                definition: compiled.name.clone(),
                issues: issues.into_boxed_slice(),
            });
        }
        let external_events = events
            .authored_names()
            .iter()
            .map(|name| EventSet::for_name(&compiled.events, name))
            .collect();
        let linked_effects = compiled
            .effect_names()
            .map(|name| effects.get(name).expect("effects were validated"))
            .collect();
        let linked_activities = compiled
            .activities
            .iter()
            .map(|activity| {
                activities
                    .get(&activity.name)
                    .expect("activities were validated")
            })
            .collect();
        Ok(Self {
            compiled,
            external_events,
            effects: linked_effects,
            activities: linked_activities,
        })
    }

    pub fn external_event(&self, index: usize) -> Option<&EventSet> {
        self.external_events.get(index)
    }

    pub fn effect(&self, effect: EffectId) -> &Arc<dyn RegisteredEffect<D>> {
        &self.effects[effect.index()]
    }

    pub fn activity(&self, activity: ActivityId) -> &Arc<dyn RegisteredActivity<D>> {
        &self.activities[activity.index()]
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
    #[error("definition declares domain `{declared}`, but this instance uses `{expected}`")]
    DomainMismatch {
        declared: Box<str>,
        expected: Box<str>,
        span_start: Option<usize>,
    },
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
    #[error("activity `{activity}` is not registered for this domain")]
    MissingActivity {
        activity: Box<str>,
        span_start: Option<usize>,
    },
}

fn event_pattern_name(pattern: &EventPattern) -> &str {
    match pattern {
        EventPattern::Exact(name) | EventPattern::Descriptor(name) => name,
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

    /// Returns the stable authored domain identifier, when one was declared.
    pub fn domain(&self) -> Option<&str> {
        self.compiled.domain.as_deref()
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

    /// Returns the shallow-history name associated with an ID from this definition.
    pub fn history_name(&self, history: HistoryId) -> Option<&str> {
        if history.definition != self.compiled.identity {
            return None;
        }
        self.compiled
            .histories
            .get(history.index())
            .map(|history| history.name.as_ref())
    }

    /// Returns the number of shallow-history declarations in the definition.
    pub fn history_count(&self) -> usize {
        self.compiled.histories.len()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DraftKind {
    Atomic,
    Compound { initial: Option<StateTreeTarget> },
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
    entry_actions_set: bool,
    exit_actions_set: bool,
    initial_actions_set: bool,
    transitions: Vec<DraftTransition>,
    activities: Vec<DraftActivity>,
}

#[derive(Clone, Debug)]
struct DraftActivity {
    id: Box<str>,
    name: Box<str>,
    arguments: Vec<(Box<str>, ExpressionId)>,
    span_start: usize,
}

#[derive(Clone, Debug)]
struct DraftTransition {
    trigger: DraftTransitionTrigger,
    condition: Option<ExpressionId>,
    target: Option<StateTreeTarget>,
    internal: bool,
    actions: Vec<DraftAction>,
    span_start: Option<u32>,
}

#[derive(Clone, Debug)]
struct DraftHistory {
    name: Box<str>,
    owner: StateId,
    default: Option<StateId>,
    actions: Vec<DraftAction>,
    actions_set: bool,
}

#[derive(Clone, Debug)]
enum DraftTransitionTrigger {
    Event(EventPattern),
    Eventless,
    After(Duration),
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

#[derive(Clone, Debug)]
struct AuthoredExpression {
    source: Box<str>,
    span_start: usize,
}

/// One declaration in an instance-local Luau datamodel.
#[derive(Clone, Debug)]
pub struct StateTreeData {
    name: Box<str>,
    initializer: Option<AuthoredExpression>,
    span_start: usize,
}

impl StateTreeData {
    /// Declares a value initialized to Luau `nil`.
    pub fn new(name: impl Into<Box<str>>) -> Self {
        Self {
            name: name.into(),
            initializer: None,
            span_start: 0,
        }
    }

    /// Returns the declared datamodel name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Sets the Luau expression evaluated when an instance initializes.
    pub fn with_initializer(mut self, expression: impl Into<Box<str>>) -> Self {
        self.initializer = Some(AuthoredExpression {
            source: expression.into(),
            span_start: self.span_start,
        });
        self
    }

    pub(crate) fn at(mut self, span_start: usize) -> Self {
        self.span_start = span_start;
        if let Some(initializer) = &mut self.initializer {
            initializer.span_start = span_start;
        }
        self
    }

    pub(crate) fn with_initializer_at(
        mut self,
        expression: impl Into<Box<str>>,
        span_start: usize,
    ) -> Self {
        self.initializer = Some(AuthoredExpression {
            source: expression.into(),
            span_start,
        });
        self
    }
}

/// One named Luau argument supplied to a typed effect or activity.
#[derive(Clone, Debug)]
pub struct StateTreeArgument {
    name: Box<str>,
    expression: AuthoredExpression,
}

impl StateTreeArgument {
    pub fn new(name: impl Into<Box<str>>, expression: impl Into<Box<str>>) -> Self {
        Self {
            name: name.into(),
            expression: AuthoredExpression {
                source: expression.into(),
                span_start: 0,
            },
        }
    }

    pub(crate) fn at(
        name: impl Into<Box<str>>,
        expression: impl Into<Box<str>>,
        span_start: usize,
    ) -> Self {
        Self {
            name: name.into(),
            expression: AuthoredExpression {
                source: expression.into(),
                span_start,
            },
        }
    }
}

pub type StateTreeEffectArgument = StateTreeArgument;

/// One structural activity attached to a state or parallel node.
#[derive(Clone, Debug)]
pub struct StateTreeActivityDefinition {
    id: Box<str>,
    name: Box<str>,
    arguments: Vec<StateTreeArgument>,
    span_start: usize,
}

impl StateTreeActivityDefinition {
    pub fn new(id: impl Into<Box<str>>, name: impl Into<Box<str>>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments: Vec::new(),
            span_start: 0,
        }
    }

    pub fn arguments(mut self, arguments: impl IntoIterator<Item = StateTreeArgument>) -> Self {
        self.arguments = arguments.into_iter().collect();
        self
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn at(mut self, span_start: usize) -> Self {
        self.span_start = span_start;
        self
    }
}

#[derive(Clone, Debug)]
enum StateTreeActionKind {
    Raise(Box<str>),
    Assign {
        location: Box<str>,
        expression: AuthoredExpression,
    },
    Effect {
        name: Box<str>,
        arguments: Vec<StateTreeEffectArgument>,
        span_start: usize,
    },
}

/// Declarative executable content attached to a state or transition.
#[derive(Clone, Debug)]
pub struct StateTreeAction {
    kind: StateTreeActionKind,
}

impl StateTreeAction {
    /// Raises one payload-free event on the instance's internal queue.
    pub fn raise(event: impl Into<Box<str>>) -> Self {
        Self {
            kind: StateTreeActionKind::Raise(event.into()),
        }
    }

    /// Assigns the result of a Luau expression to a declared datamodel value.
    pub fn assign(location: impl Into<Box<str>>, expression: impl Into<Box<str>>) -> Self {
        Self::assign_at(location, expression, 0)
    }

    /// Emits one registered typed effect after evaluating its named arguments.
    pub fn effect(
        name: impl Into<Box<str>>,
        arguments: impl IntoIterator<Item = StateTreeEffectArgument>,
    ) -> Self {
        Self::effect_at(name, arguments, 0)
    }

    pub(crate) fn assign_at(
        location: impl Into<Box<str>>,
        expression: impl Into<Box<str>>,
        span_start: usize,
    ) -> Self {
        Self {
            kind: StateTreeActionKind::Assign {
                location: location.into(),
                expression: AuthoredExpression {
                    source: expression.into(),
                    span_start,
                },
            },
        }
    }

    pub(crate) fn effect_at(
        name: impl Into<Box<str>>,
        arguments: impl IntoIterator<Item = StateTreeEffectArgument>,
        span_start: usize,
    ) -> Self {
        Self {
            kind: StateTreeActionKind::Effect {
                name: name.into(),
                arguments: arguments.into_iter().collect(),
                span_start,
            },
        }
    }
}

#[derive(Clone, Debug)]
enum StateTreeTransitionTrigger {
    EventDescriptors(Vec<Box<str>>),
    Eventless,
    After(Duration),
}

/// A complete transition authored through [`StateTreeBuilder`].
#[derive(Clone, Debug)]
pub struct StateTreeTransition {
    trigger: StateTreeTransitionTrigger,
    condition: Option<AuthoredExpression>,
    target: Option<StateTreeTarget>,
    internal: bool,
    actions: Vec<StateTreeAction>,
    span_start: usize,
}

impl StateTreeTransition {
    /// Creates a transition selected by one exact event name.
    pub fn on(event: impl Into<Box<str>>) -> Self {
        Self::on_any([event])
    }

    /// Creates a transition selected by any of the supplied event descriptors.
    pub fn on_any<T>(events: impl IntoIterator<Item = T>) -> Self
    where
        T: Into<Box<str>>,
    {
        Self {
            trigger: StateTreeTransitionTrigger::EventDescriptors(
                events.into_iter().map(Into::into).collect(),
            ),
            condition: None,
            target: None,
            internal: false,
            actions: Vec::new(),
            span_start: 0,
        }
    }

    /// Creates a transition considered while the instance stabilizes.
    pub fn eventless() -> Self {
        Self {
            trigger: StateTreeTransitionTrigger::Eventless,
            condition: None,
            target: None,
            internal: false,
            actions: Vec::new(),
            span_start: 0,
        }
    }

    /// Creates a one-shot transition triggered after its source state has
    /// remained active for `duration`.
    ///
    /// The timer is armed on state entry and cancelled on state exit. Re-entering
    /// the state starts a fresh timer.
    pub fn after(duration: Duration) -> Self {
        Self {
            trigger: StateTreeTransitionTrigger::After(duration),
            condition: None,
            target: None,
            internal: false,
            actions: Vec::new(),
            span_start: 0,
        }
    }

    /// Adds a Luau guard expression.
    pub fn condition(mut self, expression: impl Into<Box<str>>) -> Self {
        self.condition = Some(AuthoredExpression {
            source: expression.into(),
            span_start: self.span_start,
        });
        self
    }

    /// Sets the transition target. A transition without a target only runs actions.
    pub fn target(mut self, target: impl Into<StateTreeTarget>) -> Self {
        self.target = Some(target.into());
        self
    }

    /// Uses SCXML internal-transition semantics when a compound source targets
    /// one of its descendants.
    ///
    /// Such a transition preserves the source state and exits only the active
    /// descendants below it. For all other source-target relationships, SCXML
    /// defines an internal transition to behave like an external transition.
    pub fn internal(mut self) -> Self {
        self.internal = true;
        self
    }

    /// Sets the transition's executable content in execution order.
    pub fn actions(mut self, actions: impl IntoIterator<Item = StateTreeAction>) -> Self {
        self.actions = actions.into_iter().collect();
        self
    }

    pub(crate) fn at(mut self, span_start: usize) -> Self {
        self.span_start = span_start;
        self
    }

    pub(crate) fn condition_at(
        mut self,
        expression: impl Into<Box<str>>,
        span_start: usize,
    ) -> Self {
        self.condition = Some(AuthoredExpression {
            source: expression.into(),
            span_start,
        });
        self
    }
}

/// Programmatically constructs the compiler-supported SCXML semantic subset.
///
/// This is a low-level compilation target for generated definitions. Parsers
/// such as an SCXML loader should use it instead of constructing runtime state
/// directly.
#[derive(Debug)]
pub struct StateTreeBuilder {
    identity: u64,
    name: Box<str>,
    domain: Option<Box<str>>,
    domain_span_start: Option<usize>,
    states: Vec<DraftState>,
    histories: Vec<DraftHistory>,
    names: HashSet<Box<str>>,
    root: Option<StateId>,
    datamodel: Option<DraftLuauDataModel>,
}

impl StateTreeBuilder {
    /// Starts a generated definition with an authored diagnostic name.
    pub fn new(name: impl Into<Box<str>>) -> Self {
        Self {
            identity: NEXT_DEFINITION_ID.fetch_add(1, Ordering::Relaxed),
            name: name.into(),
            domain: None,
            domain_span_start: None,
            states: Vec::new(),
            histories: Vec::new(),
            names: HashSet::new(),
            root: None,
            datamodel: None,
        }
    }

    /// Restricts this generated definition to one authored domain identifier.
    pub fn domain(&mut self, domain: impl Into<Box<str>>) -> &mut Self {
        self.set_domain(domain, None)
    }

    pub(crate) fn domain_at(
        &mut self,
        domain: impl Into<Box<str>>,
        span_start: usize,
    ) -> &mut Self {
        self.set_domain(domain, Some(span_start))
    }

    fn set_domain(&mut self, domain: impl Into<Box<str>>, span_start: Option<usize>) -> &mut Self {
        self.domain = Some(domain.into());
        self.domain_span_start = span_start;
        self
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
    pub fn initial(
        &mut self,
        compound: StateId,
        target: impl Into<StateTreeTarget>,
    ) -> Result<&mut Self, BuildError> {
        let target = target.into();
        match target {
            StateTreeTarget::State(child) => {
                self.require_state(child)?;
                if self.states[child.index()].parent != Some(compound) {
                    return Err(BuildError::InitialNotDirectChild { compound, child });
                }
            }
            StateTreeTarget::History(history) => {
                let draft = self.require_history(history)?;
                if draft.owner != compound {
                    return Err(BuildError::InitialHistoryOwnedByAnotherState {
                        compound,
                        history,
                    });
                }
            }
        }

        let state = self.require_state_mut(compound)?;
        let DraftKind::Compound { initial } = &mut state.kind else {
            return Err(BuildError::InitialOnNonCompound(compound));
        };
        if initial.is_some() {
            return Err(BuildError::InitialAlreadySet(compound));
        }
        *initial = Some(target);
        Ok(self)
    }

    /// Adds a shallow-history pseudostate owned by a compound or parallel state.
    pub fn shallow_history(
        &mut self,
        name: impl Into<Box<str>>,
        owner: StateId,
    ) -> Result<HistoryId, BuildError> {
        let owner_state = self.require_state(owner)?;
        if owner_state.parent.is_none() {
            return Err(BuildError::HistoryOnRoot(owner));
        }
        if !matches!(
            owner_state.kind,
            DraftKind::Compound { .. } | DraftKind::Parallel
        ) {
            return Err(BuildError::HistoryOnNonContainer(owner));
        }
        let name = name.into();
        if name.is_empty() {
            return Err(BuildError::EmptyHistoryName);
        }
        if self.names.contains(name.as_ref()) {
            return Err(BuildError::DuplicateHistoryName(name));
        }
        if self.histories.len() >= u32::MAX as usize {
            return Err(BuildError::TooManyHistories);
        }
        let id = HistoryId {
            definition: self.identity,
            index: u32::try_from(self.histories.len()).map_err(|_| BuildError::TooManyHistories)?,
        };
        self.histories.push(DraftHistory {
            name: name.clone(),
            owner,
            default: None,
            actions: Vec::new(),
            actions_set: false,
        });
        self.names.insert(name);
        Ok(id)
    }

    /// Sets the history default to one direct child of its owner.
    pub fn history_default(
        &mut self,
        history: HistoryId,
        child: StateId,
    ) -> Result<&mut Self, BuildError> {
        self.require_state(child)?;
        let draft = self.require_history(history)?;
        if self.states[child.index()].parent != Some(draft.owner) {
            return Err(BuildError::HistoryDefaultNotDirectChild { history, child });
        }
        if draft.default.is_some() {
            return Err(BuildError::HistoryDefaultAlreadySet(history));
        }
        self.histories[history.index()].default = Some(child);
        Ok(self)
    }

    /// Sets executable content run when a history target uses its default.
    pub fn history_actions(
        &mut self,
        history: HistoryId,
        actions: impl IntoIterator<Item = StateTreeAction>,
    ) -> Result<&mut Self, BuildError> {
        let draft = self.require_history(history)?;
        if draft.actions_set {
            return Err(BuildError::HistoryActionsAlreadySet(history));
        }
        let expression_checkpoint = self.expression_count();
        let actions = match self.lower_actions(actions.into_iter().collect()) {
            Ok(actions) => actions,
            Err(error) => {
                self.truncate_expressions(expression_checkpoint);
                return Err(error);
            }
        };
        self.histories[history.index()].actions = actions;
        self.histories[history.index()].actions_set = true;
        Ok(self)
    }

    /// Declares the complete ordered root Luau datamodel.
    ///
    /// Calling this with an empty iterator enables Luau expressions without
    /// declaring persistent values.
    pub fn luau_datamodel(
        &mut self,
        declarations: impl IntoIterator<Item = StateTreeData>,
    ) -> Result<&mut Self, BuildError> {
        if self.datamodel.is_some() {
            return Err(BuildError::MultipleDataModels);
        }
        let declarations: Vec<_> = declarations.into_iter().collect();
        let mut names = Vec::with_capacity(declarations.len());
        let mut initializers = Vec::with_capacity(declarations.len());
        let mut seen = HashSet::with_capacity(declarations.len());
        for declaration in declarations {
            if !is_luau_identifier(&declaration.name) {
                return Err(BuildError::InvalidDataName {
                    name: declaration.name,
                    span_start: declaration.span_start,
                    reason: "is not a Luau identifier",
                });
            }
            if is_scxml_system_name(&declaration.name) {
                return Err(BuildError::InvalidDataName {
                    name: declaration.name,
                    span_start: declaration.span_start,
                    reason: "is reserved by the state-tree runtime",
                });
            }
            if !seen.insert(declaration.name.clone()) {
                return Err(BuildError::DuplicateDataName {
                    name: declaration.name,
                    span_start: declaration.span_start,
                });
            }
            let initializer = declaration
                .initializer
                .map(|expression| compile_authored_expression(expression, false))
                .transpose()?;
            names.push(declaration.name);
            initializers.push(initializer);
        }
        self.set_compiled_luau_datamodel(names, initializers)?;
        Ok(self)
    }

    /// Adds a complete SCXML-style transition specification.
    pub fn add_transition(
        &mut self,
        source: StateId,
        transition: StateTreeTransition,
    ) -> Result<&mut Self, BuildError> {
        if matches!(self.require_state(source)?.kind, DraftKind::Final) {
            return Err(BuildError::FinalHasTransition(source));
        }
        if let Some(target) = transition.target {
            self.require_target(target)?;
        }
        match &transition.trigger {
            StateTreeTransitionTrigger::EventDescriptors(events) => {
                if events.is_empty() {
                    return Err(BuildError::MissingEventDescriptor);
                }
                if let Some(event) = events
                    .iter()
                    .find(|event| event.contains(char::is_whitespace) || event.contains('*'))
                {
                    return Err(BuildError::InvalidEventDescriptor(event.clone()));
                }
                if events.iter().any(|event| event.is_empty()) {
                    return Err(BuildError::EmptyEventName);
                }
            }
            StateTreeTransitionTrigger::Eventless
                if transition.condition.is_none() && transition.target.is_none() =>
            {
                return Err(BuildError::EmptyTransition);
            }
            StateTreeTransitionTrigger::Eventless => {}
            StateTreeTransitionTrigger::After(duration) if duration.is_zero() => {
                return Err(BuildError::ZeroTimerDuration);
            }
            StateTreeTransitionTrigger::After(_) => {}
        }
        let expression_checkpoint = self.expression_count();
        let condition = transition
            .condition
            .map(|condition| self.register_expression(condition, true))
            .transpose();
        let condition = match condition {
            Ok(condition) => condition,
            Err(error) => {
                self.truncate_expressions(expression_checkpoint);
                return Err(error);
            }
        };
        let actions = match self.lower_actions(transition.actions) {
            Ok(actions) => actions,
            Err(error) => {
                self.truncate_expressions(expression_checkpoint);
                return Err(error);
            }
        };
        let span_start = u32::try_from(transition.span_start).ok();

        match transition.trigger {
            StateTreeTransitionTrigger::EventDescriptors(events) => {
                for event in events {
                    self.push_transition(
                        source,
                        DraftTransitionTrigger::Event(EventPattern::Descriptor(event)),
                        condition,
                        transition
                            .target
                            .map(|target| (target, transition.internal)),
                        actions.clone(),
                        span_start,
                    )?;
                }
            }
            StateTreeTransitionTrigger::Eventless => {
                self.push_transition(
                    source,
                    DraftTransitionTrigger::Eventless,
                    condition,
                    transition
                        .target
                        .map(|target| (target, transition.internal)),
                    actions,
                    span_start,
                )?;
            }
            StateTreeTransitionTrigger::After(duration) => {
                self.push_transition(
                    source,
                    DraftTransitionTrigger::After(duration),
                    condition,
                    transition
                        .target
                        .map(|target| (target, transition.internal)),
                    actions,
                    span_start,
                )?;
            }
        }
        Ok(self)
    }

    /// Sets all entry actions for a state in execution order.
    pub fn entry_actions(
        &mut self,
        state: StateId,
        actions: impl IntoIterator<Item = StateTreeAction>,
    ) -> Result<&mut Self, BuildError> {
        self.require_state(state)?;
        if self.states[state.index()].entry_actions_set {
            return Err(BuildError::ActionsAlreadySet {
                state,
                location: "entry",
            });
        }
        let expression_checkpoint = self.expression_count();
        let actions = match self.lower_actions(actions.into_iter().collect()) {
            Ok(actions) => actions,
            Err(error) => {
                self.truncate_expressions(expression_checkpoint);
                return Err(error);
            }
        };
        self.states[state.index()].entry_actions = actions;
        self.states[state.index()].entry_actions_set = true;
        Ok(self)
    }

    /// Sets all exit actions for a state in execution order.
    pub fn exit_actions(
        &mut self,
        state: StateId,
        actions: impl IntoIterator<Item = StateTreeAction>,
    ) -> Result<&mut Self, BuildError> {
        self.require_state(state)?;
        if self.states[state.index()].exit_actions_set {
            return Err(BuildError::ActionsAlreadySet {
                state,
                location: "exit",
            });
        }
        let expression_checkpoint = self.expression_count();
        let actions = match self.lower_actions(actions.into_iter().collect()) {
            Ok(actions) => actions,
            Err(error) => {
                self.truncate_expressions(expression_checkpoint);
                return Err(error);
            }
        };
        self.states[state.index()].exit_actions = actions;
        self.states[state.index()].exit_actions_set = true;
        Ok(self)
    }

    /// Sets actions run after entering a compound state and before its initial child.
    pub fn initial_actions(
        &mut self,
        state: StateId,
        actions: impl IntoIterator<Item = StateTreeAction>,
    ) -> Result<&mut Self, BuildError> {
        self.require_state(state)?;
        if !matches!(self.states[state.index()].kind, DraftKind::Compound { .. }) {
            return Err(BuildError::InitialOnNonCompound(state));
        }
        if self.states[state.index()].initial_actions_set {
            return Err(BuildError::ActionsAlreadySet {
                state,
                location: "initial",
            });
        }
        let expression_checkpoint = self.expression_count();
        let actions = match self.lower_actions(actions.into_iter().collect()) {
            Ok(actions) => actions,
            Err(error) => {
                self.truncate_expressions(expression_checkpoint);
                return Err(error);
            }
        };
        self.states[state.index()].initial_actions = actions;
        self.states[state.index()].initial_actions_set = true;
        Ok(self)
    }

    /// Adds a structural activity started whenever `owner` is entered.
    pub fn add_activity(
        &mut self,
        owner: StateId,
        activity: StateTreeActivityDefinition,
    ) -> Result<&mut Self, BuildError> {
        self.require_state(owner)?;
        if matches!(self.states[owner.index()].kind, DraftKind::Final) {
            return Err(BuildError::ActivityOnFinal(owner));
        }
        if activity.id.is_empty() {
            return Err(BuildError::EmptyActivityId);
        }
        if !is_activity_id(&activity.id) {
            return Err(BuildError::InvalidActivityId(activity.id));
        }
        if activity.name.trim().is_empty() {
            return Err(BuildError::EmptyActivityName);
        }
        if self
            .states
            .iter()
            .flat_map(|state| &state.activities)
            .any(|candidate| candidate.id == activity.id)
        {
            return Err(BuildError::DuplicateActivityId(activity.id));
        }
        if !activity.arguments.is_empty() && self.datamodel.is_none() {
            return Err(BuildError::MissingDataModel);
        }
        let checkpoint = self.expression_count();
        let mut seen = HashSet::with_capacity(activity.arguments.len());
        let mut arguments = Vec::with_capacity(activity.arguments.len());
        for argument in activity.arguments {
            if argument.name.trim().is_empty() {
                self.truncate_expressions(checkpoint);
                return Err(BuildError::EmptyActivityArgumentName);
            }
            if !seen.insert(argument.name.clone()) {
                self.truncate_expressions(checkpoint);
                return Err(BuildError::DuplicateActivityArgument(argument.name));
            }
            match self.register_expression(argument.expression, false) {
                Ok(expression) => arguments.push((argument.name, expression)),
                Err(error) => {
                    self.truncate_expressions(checkpoint);
                    return Err(error);
                }
            }
        }
        self.states[owner.index()].activities.push(DraftActivity {
            id: activity.id,
            name: activity.name,
            arguments,
            span_start: activity.span_start,
        });
        Ok(self)
    }

    fn expression_count(&self) -> usize {
        self.datamodel
            .as_ref()
            .map_or(0, |datamodel| datamodel.expressions.len())
    }

    fn truncate_expressions(&mut self, len: usize) {
        if let Some(datamodel) = &mut self.datamodel {
            datamodel.expressions.truncate(len);
        }
    }

    fn register_expression(
        &mut self,
        expression: AuthoredExpression,
        boolean: bool,
    ) -> Result<ExpressionId, BuildError> {
        let expression = compile_authored_expression(expression, boolean)?;
        self.luau_expression(expression)
    }

    fn lower_actions(
        &mut self,
        actions: Vec<StateTreeAction>,
    ) -> Result<Vec<DraftAction>, BuildError> {
        let mut lowered = Vec::with_capacity(actions.len());
        for action in actions {
            lowered.push(match action.kind {
                StateTreeActionKind::Raise(event) => {
                    if event.trim().is_empty() {
                        return Err(BuildError::EmptyEventName);
                    }
                    DraftAction::Raise(event)
                }
                StateTreeActionKind::Assign {
                    location,
                    expression,
                } => {
                    let data = self
                        .datamodel
                        .as_ref()
                        .and_then(|datamodel| {
                            datamodel.names.iter().position(|name| name == &location)
                        })
                        .and_then(|index| u32::try_from(index).ok())
                        .map(DataId)
                        .ok_or_else(|| BuildError::UnknownDataName {
                            name: location.clone(),
                            span_start: expression.span_start,
                        })?;
                    let expression = self.register_expression(expression, false)?;
                    DraftAction::Assign { data, expression }
                }
                StateTreeActionKind::Effect {
                    name,
                    arguments,
                    span_start,
                } => {
                    if name.trim().is_empty() {
                        return Err(BuildError::EmptyEffectName);
                    }
                    if !arguments.is_empty() && self.datamodel.is_none() {
                        return Err(BuildError::MissingDataModel);
                    }
                    let mut seen = HashSet::with_capacity(arguments.len());
                    let mut lowered_arguments = Vec::with_capacity(arguments.len());
                    for argument in arguments {
                        if argument.name.trim().is_empty() {
                            return Err(BuildError::EmptyEffectArgumentName);
                        }
                        if !seen.insert(argument.name.clone()) {
                            return Err(BuildError::DuplicateEffectArgument(argument.name));
                        }
                        lowered_arguments.push((
                            argument.name,
                            self.register_expression(argument.expression, false)?,
                        ));
                    }
                    DraftAction::Effect {
                        name,
                        arguments: lowered_arguments,
                        span_start,
                    }
                }
            });
        }
        Ok(lowered)
    }

    fn set_compiled_luau_datamodel(
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

    fn luau_expression(
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
        if self.domain.as_deref().is_some_and(str::is_empty) {
            return Err(BuildError::EmptyDomainName);
        }
        let root = self.root.ok_or(BuildError::MissingRoot)?;
        for (index, history) in self.histories.iter().enumerate() {
            if history.default.is_none() {
                return Err(BuildError::MissingHistoryDefault(HistoryId {
                    definition: self.identity,
                    index: u32::try_from(index).map_err(|_| BuildError::TooManyHistories)?,
                }));
            }
        }

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
        let mut timers = Vec::new();
        let mut effects = Vec::<Box<str>>::new();
        let mut effect_ids = HashMap::<Box<str>, EffectId>::new();
        let mut effect_arguments = Vec::new();
        let mut activities = Vec::new();
        let mut history_slots_by_state = vec![None; self.states.len()];
        let mut history_slot_owners = Vec::new();
        for history in &self.histories {
            if history_slots_by_state[history.owner.index()].is_none() {
                let slot = HistorySlotId(
                    u32::try_from(history_slot_owners.len())
                        .map_err(|_| BuildError::TooManyHistorySlots)?,
                );
                history_slots_by_state[history.owner.index()] = Some(slot);
                history_slot_owners.push(history.owner);
            }
        }
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
        let history_subtree_ends = (!self.histories.is_empty()).then(|| {
            let mut ends: Vec<u32> = document_orders.iter().map(|order| order + 1).collect();
            for index in (0..self.states.len()).rev() {
                if let Some(parent) = self.states[index].parent {
                    ends[parent.index()] = ends[parent.index()].max(ends[index]);
                }
            }
            ends.into_boxed_slice()
        });

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
                let trigger = match &transition.trigger {
                    DraftTransitionTrigger::Event(pattern) => {
                        let event = intern_event(&mut events, &mut event_ids, pattern.clone())?;
                        event_requirements.push(EventRequirement {
                            event,
                            span_start: transition.span_start,
                        });
                        TransitionTrigger::Event(event)
                    }
                    DraftTransitionTrigger::Eventless => TransitionTrigger::Eventless,
                    DraftTransitionTrigger::After(duration) => {
                        let timer = TimerId(
                            u32::try_from(timers.len()).map_err(|_| BuildError::TooManyTimers)?,
                        );
                        timers.push(*duration);
                        TransitionTrigger::After(timer)
                    }
                };
                let (exit_depth, entry_start, entry_len) = if let Some(target) = transition.target {
                    let effective_target = target_state(target, &self.histories);
                    let entry_start = u32::try_from(transition_entries.len())
                        .map_err(|_| BuildError::TooManyStates)?;
                    let entry_len = append_transition_entries(
                        state_id,
                        target,
                        transition.internal,
                        DraftHierarchy {
                            states: &self.states,
                            depths: &depths,
                            children: &children_by_state,
                            histories: &self.histories,
                        },
                        &mut transition_entries,
                    )?;
                    (
                        transition_exit_depth(
                            state_id,
                            effective_target,
                            transition.internal,
                            &self.states,
                            &depths,
                        ),
                        entry_start,
                        entry_len,
                    )
                } else {
                    (0, 0, 0)
                };
                transitions.push(Transition {
                    trigger,
                    condition: transition.condition,
                    target: transition.target,
                    domain: transition.target.and_then(|target| {
                        // Every legal shallow-history value is an immediate child of
                        // the owner, so its default child yields the same domain.
                        transition_domain(
                            state_id,
                            target_state(target, &self.histories),
                            transition.internal,
                            &self.states,
                            &depths,
                        )
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
                history_slot: history_slots_by_state[index],
                activity_start: u32::try_from(activities.len())
                    .map_err(|_| BuildError::TooManyActivities)?,
                activity_len: {
                    for draft_activity in &draft.activities {
                        let argument_start = u32::try_from(effect_arguments.len())
                            .map_err(|_| BuildError::TooManyEffectArguments)?;
                        effect_arguments.extend(draft_activity.arguments.iter().map(
                            |(name, expression)| NamedArgument {
                                name: Arc::from(name.as_ref()),
                                expression: *expression,
                            },
                        ));
                        let done = intern_event(
                            &mut events,
                            &mut event_ids,
                            EventPattern::Exact(
                                format!("done.activity.{}", draft_activity.id).into(),
                            ),
                        )?;
                        let error = intern_event(
                            &mut events,
                            &mut event_ids,
                            EventPattern::Exact(
                                format!("error.activity.{}", draft_activity.id).into(),
                            ),
                        )?;
                        activities.push(ActivityDefinition {
                            id: draft_activity.id.clone(),
                            name: draft_activity.name.clone(),
                            owner: state_id,
                            argument_start,
                            argument_len: u32::try_from(draft_activity.arguments.len())
                                .map_err(|_| BuildError::TooManyEffectArguments)?,
                            done,
                            error,
                            span_start: u32::try_from(draft_activity.span_start)
                                .unwrap_or(u32::MAX),
                        });
                    }
                    u32::try_from(draft.activities.len())
                        .map_err(|_| BuildError::TooManyActivities)?
                },
                transitions: transitions.into_boxed_slice(),
            });
        }

        let mut initial_entries = Vec::new();
        append_entry_configuration(
            root,
            None,
            &self.states,
            &children_by_state,
            &self.histories,
            &mut initial_entries,
        );

        let mut history_entries = Vec::new();
        let mut history_child_plans = Vec::new();
        let mut history_slots = Vec::with_capacity(history_slot_owners.len());
        for owner in history_slot_owners {
            let child_plan_start =
                u32::try_from(history_child_plans.len()).map_err(|_| BuildError::TooManyStates)?;
            for child in &children_by_state[owner.index()] {
                let entry_start =
                    u32::try_from(history_entries.len()).map_err(|_| BuildError::TooManyStates)?;
                append_entry_configuration(
                    *child,
                    None,
                    &self.states,
                    &children_by_state,
                    &self.histories,
                    &mut history_entries,
                );
                history_child_plans.push(HistoryChildPlan {
                    child: *child,
                    entry_start,
                    entry_len: u32::try_from(history_entries.len())
                        .map_err(|_| BuildError::TooManyStates)?
                        - entry_start,
                });
            }
            history_slots.push(HistorySlotDefinition {
                owner,
                child_plan_start,
                child_plan_len: u32::try_from(history_child_plans.len())
                    .map_err(|_| BuildError::TooManyStates)?
                    - child_plan_start,
            });
        }

        let mut histories = Vec::with_capacity(self.histories.len());
        for draft in &self.histories {
            let default = draft.default.expect("history defaults were validated");
            let entry_start =
                u32::try_from(history_entries.len()).map_err(|_| BuildError::TooManyStates)?;
            if self.states[draft.owner.index()].kind == DraftKind::Parallel {
                for child in &children_by_state[draft.owner.index()] {
                    append_entry_configuration(
                        *child,
                        None,
                        &self.states,
                        &children_by_state,
                        &self.histories,
                        &mut history_entries,
                    );
                }
            } else {
                append_entry_configuration(
                    default,
                    None,
                    &self.states,
                    &children_by_state,
                    &self.histories,
                    &mut history_entries,
                );
            }
            let action_start =
                u32::try_from(transition_actions.len()).map_err(|_| BuildError::TooManyActions)?;
            let action_len = append_actions(
                &draft.actions,
                &mut events,
                &mut event_ids,
                &mut effects,
                &mut effect_ids,
                &mut effect_arguments,
                &mut transition_actions,
            )?;
            histories.push(HistoryDefinition {
                name: draft.name.clone(),
                owner: draft.owner,
                slot: history_slots_by_state[draft.owner.index()]
                    .expect("a history owner has a capture slot"),
                default,
                action_start,
                action_len,
                entry_start,
                entry_len: u32::try_from(history_entries.len())
                    .map_err(|_| BuildError::TooManyStates)?
                    - entry_start,
            });
        }
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
        for activity in &activities {
            internal_events.push(activity.done);
            internal_events.push(activity.error);
        }

        Ok(StateTreeDefinition {
            compiled: Arc::new(CompiledDefinition {
                identity: self.identity,
                name: self.name,
                domain: self.domain,
                domain_span_start: self.domain_span_start,
                root,
                states: states.into_boxed_slice(),
                children: children.into_boxed_slice(),
                transition_entries: transition_entries.into_boxed_slice(),
                transition_actions: transition_actions.into_boxed_slice(),
                named_arguments: effect_arguments.into_boxed_slice(),
                activities: activities.into_boxed_slice(),
                histories: histories.into_boxed_slice(),
                history_slots: history_slots.into_boxed_slice(),
                history_child_plans: history_child_plans.into_boxed_slice(),
                history_entries: history_entries.into_boxed_slice(),
                history_subtree_ends,
                initial_entries: initial_entries.into_boxed_slice(),
                max_depth,
                max_active: max_configurations[root.index()],
                has_parallel,
                datamodel,
                timers: timers.into_boxed_slice(),
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
        if self.names.contains(name.as_ref()) {
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
            entry_actions_set: false,
            exit_actions_set: false,
            initial_actions_set: false,
            transitions: Vec::new(),
            activities: Vec::new(),
        });
        self.names.insert(name);
        Ok(id)
    }

    fn push_transition(
        &mut self,
        source: StateId,
        trigger: DraftTransitionTrigger,
        condition: Option<ExpressionId>,
        target: Option<(StateTreeTarget, bool)>,
        actions: Vec<DraftAction>,
        span_start: Option<u32>,
    ) -> Result<&mut Self, BuildError> {
        if let Some((target, _)) = target {
            self.require_target(target)?;
        }
        let (target, internal) =
            target.map_or((None, false), |(target, internal)| (Some(target), internal));
        let state = self.require_state_mut(source)?;
        if matches!(state.kind, DraftKind::Final) {
            return Err(BuildError::FinalHasTransition(source));
        }
        state.transitions.push(DraftTransition {
            trigger,
            condition,
            target,
            internal,
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

    fn require_history(&self, id: HistoryId) -> Result<&DraftHistory, BuildError> {
        if id.definition != self.identity {
            return Err(BuildError::ForeignHistory(id));
        }
        self.histories
            .get(id.index())
            .ok_or(BuildError::UnknownHistory(id))
    }

    fn require_target(&self, target: StateTreeTarget) -> Result<(), BuildError> {
        match target {
            StateTreeTarget::State(state) => self.require_state(state).map(|_| ()),
            StateTreeTarget::History(history) => self.require_history(history).map(|_| ()),
        }
    }
}

fn target_state(target: StateTreeTarget, histories: &[DraftHistory]) -> StateId {
    match target {
        StateTreeTarget::State(state) => state,
        StateTreeTarget::History(history) => histories[history.index()]
            .default
            .expect("history defaults were validated"),
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
    effect_arguments: &mut Vec<NamedArgument>,
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
    effect_arguments: &mut Vec<NamedArgument>,
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
    effect_arguments: &mut Vec<NamedArgument>,
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
            effect_arguments.extend(arguments.iter().map(|(name, expression)| NamedArgument {
                name: Arc::from(name.as_ref()),
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
    internal: bool,
    states: &[DraftState],
    depths: &[u32],
) -> Option<StateId> {
    if internal
        && matches!(states[source.index()].kind, DraftKind::Compound { .. })
        && is_proper_descendant(target, source, states)
    {
        return Some(source);
    }

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
    internal: bool,
    states: &[DraftState],
    depths: &[u32],
) -> u32 {
    transition_domain(source, target, internal, states, depths)
        .map_or(0, |state| depths[state.index()] + 1)
}

#[derive(Clone, Copy)]
struct DraftHierarchy<'a> {
    states: &'a [DraftState],
    depths: &'a [u32],
    children: &'a [Vec<StateId>],
    histories: &'a [DraftHistory],
}

fn append_transition_entries(
    source: StateId,
    target: StateTreeTarget,
    internal: bool,
    hierarchy: DraftHierarchy<'_>,
    entries: &mut Vec<Entry>,
) -> Result<u32, BuildError> {
    let effective_target = target_state(target, hierarchy.histories);
    let domain = transition_domain(
        source,
        effective_target,
        internal,
        hierarchy.states,
        hierarchy.depths,
    );
    let start = entries.len();
    let mut reverse = Vec::with_capacity(hierarchy.depths[effective_target.index()] as usize + 1);
    let mut cursor = Some(effective_target);
    while cursor != domain {
        let state = cursor.expect("transition target and source share the definition root");
        reverse.push(state);
        cursor = hierarchy.states[state.index()].parent;
    }
    reverse.reverse();

    if domain.is_some_and(|state| hierarchy.states[state.index()].kind == DraftKind::Parallel) {
        let explicit = reverse.first().copied();
        for child in &hierarchy.children[domain.expect("checked domain").index()] {
            let path = (Some(*child) == explicit).then_some(reverse.as_slice());
            append_entry_configuration(
                *child,
                path,
                hierarchy.states,
                hierarchy.children,
                hierarchy.histories,
                entries,
            );
        }
    } else {
        let first = reverse
            .first()
            .copied()
            .expect("an external transition always re-enters at least its target");
        append_entry_configuration(
            first,
            Some(&reverse),
            hierarchy.states,
            hierarchy.children,
            hierarchy.histories,
            entries,
        );
    }

    if let StateTreeTarget::History(history) = target {
        let owner = hierarchy.histories[history.index()].owner;
        if let Some(entry) = entries[start..]
            .iter_mut()
            .find(|entry| entry.state == owner)
        {
            entry.history_default = Some(HistoryIndex::from_id(history));
        }
    }

    u32::try_from(entries.len() - start).map_err(|_| BuildError::TooManyStates)
}

fn is_proper_descendant(state: StateId, ancestor: StateId, states: &[DraftState]) -> bool {
    let mut cursor = states[state.index()].parent;
    while let Some(parent) = cursor {
        if parent == ancestor {
            return true;
        }
        cursor = states[parent.index()].parent;
    }
    false
}

fn append_entry_configuration(
    state: StateId,
    explicit_path: Option<&[StateId]>,
    states: &[DraftState],
    children: &[Vec<StateId>],
    histories: &[DraftHistory],
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
        let (initial_child, history_default) = match states[state.index()].kind {
            DraftKind::Compound { initial } if explicit_child.is_none() => match initial {
                Some(StateTreeTarget::State(child)) => (Some(child), None),
                Some(StateTreeTarget::History(history)) => (
                    histories[history.index()].default,
                    Some(HistoryIndex::from_id(history)),
                ),
                None => (None, None),
            },
            _ => (None, None),
        };
        let take_initial = matches!(states[state.index()].kind, DraftKind::Compound { .. })
            && explicit_child.is_none();
        entries.push(Entry {
            state,
            take_initial,
            history_default,
        });

        match states[state.index()].kind {
            DraftKind::Atomic | DraftKind::Final => {}
            DraftKind::Compound { initial } => {
                let child = explicit_child
                    .or(initial_child)
                    .or_else(|| initial.map(|target| target_state(target, histories)));
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

fn compile_authored_expression(
    expression: AuthoredExpression,
    boolean: bool,
) -> Result<CompiledLuauExpression, BuildError> {
    compile_expression(&expression.source, boolean, expression.span_start).map_err(|error| {
        BuildError::InvalidLuau {
            span_start: expression.span_start,
            message: error.to_string().into(),
        }
    })
}

pub(crate) fn is_luau_identifier(value: &str) -> bool {
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

pub(crate) fn is_scxml_system_name(value: &str) -> bool {
    matches!(
        value,
        "_event" | "_sessionid" | "_name" | "_ioprocessors" | "_x" | "In"
    )
}

pub(crate) fn is_activity_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

/// A state-tree definition validation error.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum BuildError {
    #[error("the definition name cannot be empty")]
    EmptyDefinitionName,
    #[error("the authored domain name cannot be empty")]
    EmptyDomainName,
    #[error("a state name cannot be empty")]
    EmptyStateName,
    #[error("a history name cannot be empty")]
    EmptyHistoryName,
    #[error("an event name cannot be empty")]
    EmptyEventName,
    #[error("state name `{0}` duplicates another state or history name")]
    DuplicateStateName(Box<str>),
    #[error("history name `{0}` duplicates another state or history name")]
    DuplicateHistoryName(Box<str>),
    #[error("a definition must have exactly one compound root")]
    MissingRoot,
    #[error("a definition cannot have more than one root")]
    MultipleRoots,
    #[error("state {0:?} does not exist")]
    UnknownState(StateId),
    #[error("state {0:?} belongs to another definition")]
    ForeignState(StateId),
    #[error("history {0:?} does not exist")]
    UnknownHistory(HistoryId),
    #[error("history {0:?} belongs to another definition")]
    ForeignHistory(HistoryId),
    #[error("state {0:?} cannot contain child states")]
    ChildOfNonCompound(StateId),
    #[error("state {0:?} is not compound and cannot have an initial child")]
    InitialOnNonCompound(StateId),
    #[error("state {child:?} is not a direct child of {compound:?}")]
    InitialNotDirectChild { compound: StateId, child: StateId },
    #[error("compound state {0:?} already has an initial child")]
    InitialAlreadySet(StateId),
    #[error("compound state {0:?} has no initial child")]
    MissingInitial(StateId),
    #[error("history {history:?} is not owned by compound state {compound:?}")]
    InitialHistoryOwnedByAnotherState {
        compound: StateId,
        history: HistoryId,
    },
    #[error("the synthetic root state {0:?} cannot own history")]
    HistoryOnRoot(StateId),
    #[error("state {0:?} is not compound or parallel and cannot own history")]
    HistoryOnNonContainer(StateId),
    #[error("state {child:?} is not a direct child of history {history:?}'s owner")]
    HistoryDefaultNotDirectChild { history: HistoryId, child: StateId },
    #[error("history {0:?} already has a default child")]
    HistoryDefaultAlreadySet(HistoryId),
    #[error("history {0:?} has no default child")]
    MissingHistoryDefault(HistoryId),
    #[error("default actions are already set for history {0:?}")]
    HistoryActionsAlreadySet(HistoryId),
    #[error("parallel state {0:?} has no regions")]
    ParallelHasNoRegions(StateId),
    #[error("final state {0:?} cannot have outgoing transitions")]
    FinalHasTransition(StateId),
    #[error("parallel state {0:?} cannot contain a final child")]
    FinalChildOfParallel(StateId),
    #[error("a definition cannot contain more than u32::MAX states")]
    TooManyStates,
    #[error("a definition cannot contain more than u32::MAX histories")]
    TooManyHistories,
    #[error("a definition cannot contain more than u32::MAX history capture slots")]
    TooManyHistorySlots,
    #[error("a definition cannot contain more than u32::MAX event names")]
    TooManyEvents,
    #[error("a definition cannot contain more than u32::MAX executable actions")]
    TooManyActions,
    #[error("a definition cannot contain more than one datamodel")]
    MultipleDataModels,
    #[error("a Luau expression requires a datamodel")]
    MissingDataModel,
    #[error("invalid Luau expression at byte {span_start}: {message}")]
    InvalidLuau {
        span_start: usize,
        message: Box<str>,
    },
    #[error("data name `{name}` {reason} at byte {span_start}")]
    InvalidDataName {
        name: Box<str>,
        span_start: usize,
        reason: &'static str,
    },
    #[error("data name `{name}` is duplicated at byte {span_start}")]
    DuplicateDataName { name: Box<str>, span_start: usize },
    #[error("assignment location `{name}` is not declared at byte {span_start}")]
    UnknownDataName { name: Box<str>, span_start: usize },
    #[error("a transition must contain at least one event descriptor")]
    MissingEventDescriptor,
    #[error("event descriptor `{0}` must name one exact event")]
    InvalidEventDescriptor(Box<str>),
    #[error("an eventless transition requires a condition or target")]
    EmptyTransition,
    #[error("an after transition requires a positive duration")]
    ZeroTimerDuration,
    #[error("an effect name cannot be empty")]
    EmptyEffectName,
    #[error("an effect argument name cannot be empty")]
    EmptyEffectArgumentName,
    #[error("effect argument `{0}` is duplicated")]
    DuplicateEffectArgument(Box<str>),
    #[error("{location} actions are already set for state {state:?}")]
    ActionsAlreadySet {
        state: StateId,
        location: &'static str,
    },
    #[error("a definition cannot contain more than u32::MAX data items")]
    TooManyDataItems,
    #[error("a definition cannot contain more than u32::MAX expressions")]
    TooManyExpressions,
    #[error("a definition cannot contain more than u32::MAX effects")]
    TooManyEffects,
    #[error("a definition cannot contain more than u32::MAX effect arguments")]
    TooManyEffectArguments,
    #[error("a definition cannot contain more than u32::MAX timers")]
    TooManyTimers,
    #[error("an activity id cannot be empty")]
    EmptyActivityId,
    #[error(
        "activity id `{0}` must be one event-name segment containing only ASCII letters, digits, `_`, or `-`"
    )]
    InvalidActivityId(Box<str>),
    #[error("activity id `{0}` is duplicated")]
    DuplicateActivityId(Box<str>),
    #[error("an activity name cannot be empty")]
    EmptyActivityName,
    #[error("an activity argument name cannot be empty")]
    EmptyActivityArgumentName,
    #[error("activity argument `{0}` is duplicated")]
    DuplicateActivityArgument(Box<str>),
    #[error("final state {0:?} cannot own an activity")]
    ActivityOnFinal(StateId),
    #[error("a definition cannot contain more than u32::MAX activities")]
    TooManyActivities,
}

#[cfg(test)]
// The parity projection omits only process-local identities and source offsets.
// It intentionally has no per-test configuration or field exclusions.
#[derive(Debug, Eq, PartialEq)]
struct DefinitionSemantics {
    name: Box<str>,
    domain: Option<Box<str>>,
    root: u32,
    states: Vec<StateSemantics>,
    children: Vec<u32>,
    transition_entries: Vec<EntrySemantics>,
    transition_actions: Vec<ActionSemantics>,
    named_arguments: Vec<(Arc<str>, ExpressionId)>,
    activities: Vec<ActivitySemantics>,
    histories: Vec<HistorySemantics>,
    history_slots: Vec<HistorySlotSemantics>,
    history_child_plans: Vec<HistoryChildPlanSemantics>,
    history_entries: Vec<EntrySemantics>,
    initial_entries: Vec<EntrySemantics>,
    max_depth: usize,
    max_active: usize,
    has_parallel: bool,
    datamodel: Option<DataModelSemantics>,
    timers: Vec<Duration>,
    events: Vec<EventPattern>,
    event_requirements: Vec<EventId>,
    internal_events: Vec<EventId>,
    effects: Vec<Box<str>>,
}

#[cfg(test)]
#[derive(Debug, Eq, PartialEq)]
struct ActivitySemantics {
    id: Box<str>,
    name: Box<str>,
    owner: u32,
    argument_start: u32,
    argument_len: u32,
    done: EventId,
    error: EventId,
}

#[cfg(test)]
#[derive(Debug, Eq, PartialEq)]
struct StateSemantics {
    name: Box<str>,
    parent: Option<u32>,
    depth: u32,
    document_order: u32,
    kind: StateKindSemantics,
    child_start: u32,
    child_len: u32,
    entry_actions: Vec<ActionSemantics>,
    exit_actions: Vec<ActionSemantics>,
    initial_actions: Vec<ActionSemantics>,
    history_slot: Option<u32>,
    activity_start: u32,
    activity_len: u32,
    transitions: Vec<TransitionSemantics>,
}

#[cfg(test)]
#[derive(Debug, Eq, PartialEq)]
enum StateKindSemantics {
    Atomic,
    Compound {
        initial: TargetSemantics,
        completion: EventId,
    },
    Parallel {
        completion: EventId,
    },
    Final,
}

#[cfg(test)]
#[derive(Debug, Eq, PartialEq)]
struct TransitionSemantics {
    trigger: TransitionTrigger,
    condition: Option<ExpressionId>,
    target: Option<TargetSemantics>,
    domain: Option<u32>,
    exit_depth: u32,
    entry_start: u32,
    entry_len: u32,
    action_start: u32,
    action_len: u32,
}

#[cfg(test)]
#[derive(Debug, Eq, PartialEq)]
struct EntrySemantics {
    state: u32,
    take_initial: bool,
    history_default: Option<u32>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TargetSemantics {
    State(u32),
    History(u32),
}

#[cfg(test)]
#[derive(Debug, Eq, PartialEq)]
struct HistorySemantics {
    name: Box<str>,
    owner: u32,
    slot: u32,
    default: u32,
    action_start: u32,
    action_len: u32,
    entry_start: u32,
    entry_len: u32,
}

#[cfg(test)]
#[derive(Debug, Eq, PartialEq)]
struct HistorySlotSemantics {
    owner: u32,
    child_plan_start: u32,
    child_plan_len: u32,
}

#[cfg(test)]
#[derive(Debug, Eq, PartialEq)]
struct HistoryChildPlanSemantics {
    child: u32,
    entry_start: u32,
    entry_len: u32,
}

#[cfg(test)]
#[derive(Debug, Eq, PartialEq)]
enum ActionSemantics {
    Raise(EventId),
    Assign {
        data: DataId,
        expression: ExpressionId,
    },
    Effect {
        effect: EffectId,
        argument_start: u32,
        argument_len: u32,
    },
}

#[cfg(test)]
#[derive(Debug, Eq, PartialEq)]
struct DataModelSemantics {
    names: Vec<Box<str>>,
    initializers: Vec<Option<ExpressionId>>,
    expressions: Vec<ExpressionSemantics>,
}

#[cfg(test)]
#[derive(Debug, Eq, PartialEq)]
struct ExpressionSemantics {
    bytecode: Box<[u8]>,
    reads_event_data: bool,
}

#[cfg(test)]
pub(crate) fn assert_definition_semantics_eq(
    actual: &StateTreeDefinition,
    expected: &StateTreeDefinition,
) {
    assert_eq!(definition_semantics(actual), definition_semantics(expected));
}

#[cfg(test)]
fn definition_semantics(definition: &StateTreeDefinition) -> DefinitionSemantics {
    let compiled = &definition.compiled;
    DefinitionSemantics {
        name: compiled.name.clone(),
        domain: compiled.domain.clone(),
        root: compiled.root.index,
        states: compiled
            .states
            .iter()
            .map(|state| StateSemantics {
                name: state.name.clone(),
                parent: state.parent.map(|state| state.index),
                depth: state.depth,
                document_order: state.document_order,
                kind: match state.kind {
                    StateKind::Atomic => StateKindSemantics::Atomic,
                    StateKind::Compound {
                        initial,
                        completion,
                    } => StateKindSemantics::Compound {
                        initial: target_semantics(initial),
                        completion,
                    },
                    StateKind::Parallel { completion } => {
                        StateKindSemantics::Parallel { completion }
                    }
                    StateKind::Final => StateKindSemantics::Final,
                },
                child_start: state.child_start,
                child_len: state.child_len,
                entry_actions: state.entry_actions.iter().map(action_semantics).collect(),
                exit_actions: state.exit_actions.iter().map(action_semantics).collect(),
                initial_actions: state.initial_actions.iter().map(action_semantics).collect(),
                history_slot: state.history_slot.map(|slot| slot.0),
                activity_start: state.activity_start,
                activity_len: state.activity_len,
                transitions: state
                    .transitions
                    .iter()
                    .map(|transition| TransitionSemantics {
                        trigger: transition.trigger,
                        condition: transition.condition,
                        target: transition.target.map(target_semantics),
                        domain: transition.domain.map(|state| state.index),
                        exit_depth: transition.exit_depth,
                        entry_start: transition.entry_start,
                        entry_len: transition.entry_len,
                        action_start: transition.action_start,
                        action_len: transition.action_len,
                    })
                    .collect(),
            })
            .collect(),
        children: compiled.children.iter().map(|state| state.index).collect(),
        transition_entries: compiled
            .transition_entries
            .iter()
            .map(entry_semantics)
            .collect(),
        transition_actions: compiled
            .transition_actions
            .iter()
            .map(action_semantics)
            .collect(),
        named_arguments: compiled
            .named_arguments
            .iter()
            .map(|argument| (Arc::clone(&argument.name), argument.expression))
            .collect(),
        activities: compiled
            .activities
            .iter()
            .map(|activity| ActivitySemantics {
                id: activity.id.clone(),
                name: activity.name.clone(),
                owner: activity.owner.index,
                argument_start: activity.argument_start,
                argument_len: activity.argument_len,
                done: activity.done,
                error: activity.error,
            })
            .collect(),
        histories: compiled
            .histories
            .iter()
            .map(|history| HistorySemantics {
                name: history.name.clone(),
                owner: history.owner.index,
                slot: history.slot.0,
                default: history.default.index,
                action_start: history.action_start,
                action_len: history.action_len,
                entry_start: history.entry_start,
                entry_len: history.entry_len,
            })
            .collect(),
        history_slots: compiled
            .history_slots
            .iter()
            .map(|slot| HistorySlotSemantics {
                owner: slot.owner.index,
                child_plan_start: slot.child_plan_start,
                child_plan_len: slot.child_plan_len,
            })
            .collect(),
        history_child_plans: compiled
            .history_child_plans
            .iter()
            .map(|plan| HistoryChildPlanSemantics {
                child: plan.child.index,
                entry_start: plan.entry_start,
                entry_len: plan.entry_len,
            })
            .collect(),
        history_entries: compiled
            .history_entries
            .iter()
            .map(entry_semantics)
            .collect(),
        initial_entries: compiled
            .initial_entries
            .iter()
            .map(entry_semantics)
            .collect(),
        max_depth: compiled.max_depth,
        max_active: compiled.max_active,
        has_parallel: compiled.has_parallel,
        datamodel: compiled
            .datamodel
            .as_ref()
            .map(|datamodel| DataModelSemantics {
                names: datamodel.names.to_vec(),
                initializers: datamodel.initializers.to_vec(),
                expressions: datamodel
                    .expressions
                    .iter()
                    .map(|expression| ExpressionSemantics {
                        bytecode: expression.bytecode.clone(),
                        reads_event_data: expression.reads_event_data,
                    })
                    .collect(),
            }),
        timers: compiled.timers.to_vec(),
        events: compiled.events.to_vec(),
        event_requirements: compiled
            .event_requirements
            .iter()
            .map(|requirement| requirement.event)
            .collect(),
        internal_events: compiled.internal_events.to_vec(),
        effects: compiled.effects.to_vec(),
    }
}

#[cfg(test)]
fn action_semantics(action: &Action) -> ActionSemantics {
    match *action {
        Action::Raise(event) => ActionSemantics::Raise(event),
        Action::Assign { data, expression } => ActionSemantics::Assign { data, expression },
        Action::Effect {
            effect,
            argument_start,
            argument_len,
            ..
        } => ActionSemantics::Effect {
            effect,
            argument_start,
            argument_len,
        },
    }
}

#[cfg(test)]
fn entry_semantics(entry: &Entry) -> EntrySemantics {
    EntrySemantics {
        state: entry.state.index,
        take_initial: entry.take_initial,
        history_default: entry.history_default.map(|history| history.index() as u32),
    }
}

#[cfg(test)]
fn target_semantics(target: StateTreeTarget) -> TargetSemantics {
    match target {
        StateTreeTarget::State(state) => TargetSemantics::State(state.index),
        StateTreeTarget::History(history) => TargetSemantics::History(history.index),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_definitions_may_declare_a_domain() {
        let mut builder = StateTreeBuilder::new("generated");
        builder.domain("simulation");
        let root = builder.compound_root("root").unwrap();
        let idle = builder.atomic("idle", root).unwrap();
        builder.initial(root, idle).unwrap();
        let definition = builder.build().unwrap();
        assert_eq!(definition.domain(), Some("simulation"));

        let mut invalid = StateTreeBuilder::new("invalid");
        invalid.domain("");
        let root = invalid.compound_root("root").unwrap();
        let idle = invalid.atomic("idle", root).unwrap();
        invalid.initial(root, idle).unwrap();
        assert!(matches!(invalid.build(), Err(BuildError::EmptyDomainName)));
    }

    #[test]
    fn entry_layout_remains_compact_with_history_support() {
        assert_eq!(std::mem::size_of::<Entry>(), 24);
    }

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
        builder
            .add_transition(first, StateTreeTransition::on("advance").target(second))
            .unwrap();
        builder
            .add_transition(first, StateTreeTransition::on("advance").target(first))
            .unwrap();

        let definition = builder.build().unwrap();
        assert_eq!(
            definition.compiled.state(first).transitions[0].target,
            Some(second.into())
        );
        assert_eq!(
            definition.compiled.state(first).transitions[1].target,
            Some(first.into())
        );
    }

    #[test]
    fn internal_transition_without_descendant_target_uses_external_domain() {
        fn definition(internal: bool) -> StateTreeDefinition {
            let mut builder = StateTreeBuilder::new("internal-fallback");
            let root = builder.compound_root("root").unwrap();
            let parent = builder.compound("parent", root).unwrap();
            let child = builder.atomic("child", parent).unwrap();
            let sibling = builder.atomic("sibling", root).unwrap();
            builder.initial(root, parent).unwrap();
            builder.initial(parent, child).unwrap();
            let transition = StateTreeTransition::on("advance").target(sibling);
            builder
                .add_transition(
                    parent,
                    if internal {
                        transition.internal()
                    } else {
                        transition
                    },
                )
                .unwrap();
            builder.build().unwrap()
        }

        assert_definition_semantics_eq(&definition(true), &definition(false));
    }

    #[test]
    fn rejects_replacing_an_initial_child() {
        let mut builder = StateTreeBuilder::new("duplicate-initial");
        let root = builder.compound_root("root").unwrap();
        let first = builder.atomic("first", root).unwrap();
        let second = builder.atomic("second", root).unwrap();

        builder.initial(root, first).unwrap();
        assert_eq!(
            builder.initial(root, second).unwrap_err(),
            BuildError::InitialAlreadySet(root)
        );
    }

    #[test]
    fn compiles_shallow_history_targets_and_shared_owner_slot() {
        let mut builder = StateTreeBuilder::new("history");
        let root = builder.compound_root("root").unwrap();
        let owner = builder.compound("owner", root).unwrap();
        let first = builder.atomic("first", owner).unwrap();
        let second = builder.atomic("second", owner).unwrap();
        let outside = builder.atomic("outside", root).unwrap();
        builder.initial(root, owner).unwrap();
        builder.initial(owner, first).unwrap();
        let recent = builder.shallow_history("recent", owner).unwrap();
        let fallback = builder.shallow_history("fallback", owner).unwrap();
        builder.history_default(recent, first).unwrap();
        builder.history_default(fallback, second).unwrap();
        builder
            .history_actions(recent, [StateTreeAction::raise("history.default")])
            .unwrap();
        builder
            .add_transition(outside, StateTreeTransition::on("return").target(recent))
            .unwrap();
        builder
            .add_transition(
                owner,
                StateTreeTransition::on("reset").target(recent).internal(),
            )
            .unwrap();

        let definition = builder.build().unwrap();
        let compiled = &definition.compiled;
        assert_eq!(definition.history_name(recent), Some("recent"));
        assert_eq!(definition.history_count(), 2);
        assert_eq!(compiled.histories[0].slot, compiled.histories[1].slot);
        assert_eq!(
            compiled.state(owner).history_slot,
            Some(compiled.histories[0].slot)
        );
        assert_eq!(
            compiled.state(outside).transitions[0].target,
            Some(recent.into())
        );
        assert_eq!(compiled.state(outside).transitions[0].domain, Some(root));
        assert_eq!(compiled.state(owner).transitions[0].domain, Some(owner));
        assert_eq!(compiled.histories[0].action_len, 1);
        assert_eq!(
            compiled
                .history_child_plans(compiled.histories[0].slot)
                .len(),
            2
        );
    }

    #[test]
    fn history_can_be_a_compound_initial_target() {
        let mut builder = StateTreeBuilder::new("initial-history");
        let root = builder.compound_root("root").unwrap();
        let owner = builder.compound("owner", root).unwrap();
        let child = builder.atomic("child", owner).unwrap();
        let history = builder.shallow_history("recent", owner).unwrap();
        builder.history_default(history, child).unwrap();
        builder.initial(root, owner).unwrap();
        builder.initial(owner, history).unwrap();

        let definition = builder.build().unwrap();
        let owner_entry = definition
            .compiled
            .initial_entries
            .iter()
            .find(|entry| entry.state == owner)
            .unwrap();
        assert_eq!(
            owner_entry.history_default,
            Some(HistoryIndex::from_id(history))
        );
        assert_eq!(
            definition.compiled.state(owner).kind,
            StateKind::Compound {
                initial: history.into(),
                completion: match definition.compiled.state(owner).kind {
                    StateKind::Compound { completion, .. } => completion,
                    _ => unreachable!(),
                },
            }
        );
    }

    #[test]
    fn rejects_invalid_history_owners_and_defaults() {
        let mut builder = StateTreeBuilder::new("invalid-history");
        let root = builder.compound_root("root").unwrap();
        let owner = builder.compound("owner", root).unwrap();
        let child = builder.atomic("child", owner).unwrap();
        let outside = builder.atomic("outside", root).unwrap();

        assert_eq!(
            builder.shallow_history("root-history", root),
            Err(BuildError::HistoryOnRoot(root))
        );
        assert_eq!(
            builder.shallow_history("leaf-history", child),
            Err(BuildError::HistoryOnNonContainer(child))
        );
        let history = builder.shallow_history("recent", owner).unwrap();
        assert_eq!(
            builder.history_default(history, outside).unwrap_err(),
            BuildError::HistoryDefaultNotDirectChild {
                history,
                child: outside,
            }
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
    fn rejects_invalid_event_descriptors() {
        let mut builder = StateTreeBuilder::new("invalid-event");
        let root = builder.compound_root("root").unwrap();
        let source = builder.atomic("source", root).unwrap();
        let target = builder.atomic("target", root).unwrap();

        assert_eq!(
            builder
                .add_transition(source, StateTreeTransition::on("").target(target))
                .unwrap_err(),
            BuildError::EmptyEventName,
        );
        assert_eq!(
            builder
                .add_transition(
                    source,
                    StateTreeTransition::on("advance fast").target(target),
                )
                .unwrap_err(),
            BuildError::InvalidEventDescriptor("advance fast".into())
        );
        for wildcard in ["*", "advance.*"] {
            assert_eq!(
                builder
                    .add_transition(source, StateTreeTransition::on(wildcard).target(target))
                    .unwrap_err(),
                BuildError::InvalidEventDescriptor(wildcard.into())
            );
        }
    }

    #[test]
    fn activity_ids_are_single_safe_event_name_segments() {
        for id in ["job", "job-child", "job_child"] {
            let mut builder = StateTreeBuilder::new("activity-id");
            let root = builder.compound_root("root").unwrap();
            let active = builder.atomic("active", root).unwrap();
            builder.initial(root, active).unwrap();
            builder
                .add_activity(active, StateTreeActivityDefinition::new(id, "work"))
                .unwrap();
        }

        for id in ["job.child", "*", "job child"] {
            let mut builder = StateTreeBuilder::new("invalid-activity-id");
            let root = builder.compound_root("root").unwrap();
            let active = builder.atomic("active", root).unwrap();
            builder.initial(root, active).unwrap();
            assert_eq!(
                builder
                    .add_activity(active, StateTreeActivityDefinition::new(id, "work"))
                    .unwrap_err(),
                BuildError::InvalidActivityId(id.into())
            );
        }
    }

    #[test]
    fn rejects_zero_duration_after_transitions() {
        let mut builder = StateTreeBuilder::new("zero-timeout");
        let root = builder.compound_root("root").unwrap();
        let waiting = builder.atomic("waiting", root).unwrap();

        assert_eq!(
            builder
                .add_transition(waiting, StateTreeTransition::after(Duration::ZERO))
                .unwrap_err(),
            BuildError::ZeroTimerDuration
        );
    }

    #[test]
    fn rejects_luau_keywords_and_system_data_names() {
        for name in ["if", "true", "_event", "_x", "In"] {
            let mut builder = StateTreeBuilder::new("invalid-data");
            assert!(builder.luau_datamodel([StateTreeData::new(name)]).is_err());
        }
    }

    #[test]
    fn rejected_transition_does_not_retain_expressions() {
        fn definition(after_rejection: bool) -> StateTreeDefinition {
            let mut builder = StateTreeBuilder::new("transactional");
            builder.luau_datamodel([]).unwrap();
            let root = builder.compound_root("root").unwrap();
            let state = builder.atomic("state", root).unwrap();
            builder.initial(root, state).unwrap();
            if after_rejection {
                assert_eq!(
                    builder
                        .add_transition(
                            state,
                            StateTreeTransition::on_any(std::iter::empty::<&str>())
                                .condition("true"),
                        )
                        .unwrap_err(),
                    BuildError::MissingEventDescriptor
                );
            }
            builder
                .add_transition(state, StateTreeTransition::eventless().condition("false"))
                .unwrap();
            builder.build().unwrap()
        }

        assert_definition_semantics_eq(&definition(true), &definition(false));
    }

    #[test]
    fn action_lists_can_only_be_set_once() {
        let mut builder = StateTreeBuilder::new("actions");
        let root = builder.compound_root("root").unwrap();
        let state = builder.atomic("state", root).unwrap();
        builder.entry_actions(state, []).unwrap();

        assert_eq!(
            builder
                .entry_actions(state, [StateTreeAction::raise("again")])
                .unwrap_err(),
            BuildError::ActionsAlreadySet {
                state,
                location: "entry"
            }
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
