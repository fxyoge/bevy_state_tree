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

use crate::StateTreeEvents;

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
    Final,
}

#[derive(Clone, Debug)]
pub(crate) struct StateNode {
    pub name: Box<str>,
    pub parent: Option<StateId>,
    pub depth: u32,
    pub kind: StateKind,
    pub entry_raises: Box<[EventId]>,
    pub exit_raises: Box<[EventId]>,
    pub initial_raises: Box<[EventId]>,
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
    pub target: StateId,
    pub exit_depth: u32,
    pub entry_start: u32,
    pub entry_len: u32,
    pub raise_start: u32,
    pub raise_len: u32,
}

#[derive(Clone, Debug)]
pub(crate) struct CompiledDefinition {
    pub identity: u64,
    pub name: Box<str>,
    pub root: StateId,
    pub states: Box<[StateNode]>,
    pub transition_entries: Box<[Entry]>,
    pub transition_raises: Box<[EventId]>,
    pub initial_entries: Box<[Entry]>,
    pub max_depth: usize,
    events: Box<[EventPattern]>,
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

    pub fn internal_event_matches(&self, candidate: EventId, emitted: EventId) -> bool {
        let EventPattern::Exact(emitted) = &self.events[emitted.index()] else {
            return false;
        };
        event_pattern_matches(&self.events[candidate.index()], emitted)
    }
}

#[derive(Debug)]
pub(crate) struct LinkedDefinition {
    pub compiled: Arc<CompiledDefinition>,
    external_events: Box<[EventSet]>,
}

impl LinkedDefinition {
    pub fn new<E: StateTreeEvents>(compiled: Arc<CompiledDefinition>) -> Self {
        let external_events = E::AUTHORED_NAMES
            .iter()
            .map(|name| EventSet::for_name(&compiled.events, name))
            .collect();
        Self {
            compiled,
            external_events,
        }
    }

    pub fn external_event(&self, index: usize) -> Option<&EventSet> {
        self.external_events.get(index)
    }
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
    Final,
}

#[derive(Clone, Debug)]
struct DraftState {
    name: Box<str>,
    parent: Option<StateId>,
    kind: DraftKind,
    entry_raises: Vec<Box<str>>,
    exit_raises: Vec<Box<str>>,
    initial_raises: Vec<Box<str>>,
    transitions: Vec<DraftTransition>,
}

#[derive(Clone, Debug)]
struct DraftTransition {
    event: Option<EventPattern>,
    target: StateId,
    raises: Vec<Box<str>>,
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
        if event.is_empty() {
            return Err(BuildError::EmptyEventName);
        }
        self.add_transition(source, Some(EventPattern::Exact(event)), target, Vec::new())
    }

    pub(crate) fn event_descriptor_with_raises(
        &mut self,
        source: StateId,
        descriptor: impl Into<Box<str>>,
        target: StateId,
        raises: Vec<Box<str>>,
    ) -> Result<&mut Self, BuildError> {
        let descriptor = descriptor.into();
        if descriptor.is_empty() {
            return Err(BuildError::EmptyEventName);
        }
        self.add_transition(
            source,
            Some(EventPattern::Descriptor(descriptor)),
            target,
            raises,
        )
    }

    #[cfg(test)]
    pub(crate) fn event_descriptor(
        &mut self,
        source: StateId,
        descriptor: impl Into<Box<str>>,
        target: StateId,
    ) -> Result<&mut Self, BuildError> {
        self.event_descriptor_with_raises(source, descriptor, target, Vec::new())
    }

    /// Adds an eventless external transition.
    pub fn eventless(&mut self, source: StateId, target: StateId) -> Result<&mut Self, BuildError> {
        self.add_transition(source, None, target, Vec::new())
    }

    pub(crate) fn eventless_with_raises(
        &mut self,
        source: StateId,
        target: StateId,
        raises: Vec<Box<str>>,
    ) -> Result<&mut Self, BuildError> {
        self.add_transition(source, None, target, raises)
    }

    pub(crate) fn entry_raises(
        &mut self,
        state: StateId,
        raises: Vec<Box<str>>,
    ) -> Result<&mut Self, BuildError> {
        self.require_state_mut(state)?.entry_raises = raises;
        Ok(self)
    }

    pub(crate) fn exit_raises(
        &mut self,
        state: StateId,
        raises: Vec<Box<str>>,
    ) -> Result<&mut Self, BuildError> {
        self.require_state_mut(state)?.exit_raises = raises;
        Ok(self)
    }

    pub(crate) fn initial_raises(
        &mut self,
        state: StateId,
        raises: Vec<Box<str>>,
    ) -> Result<&mut Self, BuildError> {
        self.require_state_mut(state)?.initial_raises = raises;
        Ok(self)
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
        let mut states = Vec::with_capacity(self.states.len());
        let mut transition_entries = Vec::new();
        let mut transition_raises = Vec::new();

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
                transitions.push(Transition {
                    event,
                    target: transition.target,
                    exit_depth: transition_exit_depth(
                        state_id,
                        transition.target,
                        &self.states,
                        &depths,
                    ),
                    entry_start: u32::try_from(transition_entries.len())
                        .map_err(|_| BuildError::TooManyStates)?,
                    entry_len: append_transition_entries(
                        state_id,
                        transition.target,
                        &self.states,
                        &depths,
                        &mut transition_entries,
                    )?,
                    raise_start: u32::try_from(transition_raises.len())
                        .map_err(|_| BuildError::TooManyActions)?,
                    raise_len: append_raises(
                        &transition.raises,
                        &mut events,
                        &mut event_ids,
                        &mut transition_raises,
                    )?,
                });
            }

            states.push(StateNode {
                name: draft.name.clone(),
                parent: draft.parent,
                depth: depths[index],
                kind,
                entry_raises: compile_raises(&draft.entry_raises, &mut events, &mut event_ids)?,
                exit_raises: compile_raises(&draft.exit_raises, &mut events, &mut event_ids)?,
                initial_raises: compile_raises(&draft.initial_raises, &mut events, &mut event_ids)?,
                transitions: transitions.into_boxed_slice(),
            });
        }

        let mut initial_entries = vec![Entry {
            state: root,
            take_initial: matches!(states[root.index()].kind, StateKind::Compound { .. }),
        }];
        append_initial_descendants(root, &states, &mut initial_entries);
        let max_depth = depths.iter().copied().max().unwrap_or(0).saturating_add(1) as usize;

        Ok(StateTreeDefinition {
            compiled: Arc::new(CompiledDefinition {
                identity: self.identity,
                name: self.name,
                root,
                states: states.into_boxed_slice(),
                transition_entries: transition_entries.into_boxed_slice(),
                transition_raises: transition_raises.into_boxed_slice(),
                initial_entries: initial_entries.into_boxed_slice(),
                max_depth,
                events: events.into_boxed_slice(),
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
        if !matches!(parent_state.kind, DraftKind::Compound { .. }) {
            return Err(BuildError::ChildOfNonCompound(parent));
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
            entry_raises: Vec::new(),
            exit_raises: Vec::new(),
            initial_raises: Vec::new(),
            transitions: Vec::new(),
        });
        self.names.insert(name, id);
        Ok(id)
    }

    fn add_transition(
        &mut self,
        source: StateId,
        event: Option<EventPattern>,
        target: StateId,
        raises: Vec<Box<str>>,
    ) -> Result<&mut Self, BuildError> {
        self.require_state(target)?;
        let state = self.require_state_mut(source)?;
        if matches!(state.kind, DraftKind::Final) {
            return Err(BuildError::FinalHasTransition(source));
        }
        state.transitions.push(DraftTransition {
            event,
            target,
            raises,
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

fn compile_raises(
    raises: &[Box<str>],
    events: &mut Vec<EventPattern>,
    event_ids: &mut HashMap<EventPattern, EventId>,
) -> Result<Box<[EventId]>, BuildError> {
    raises
        .iter()
        .map(|event| intern_event(events, event_ids, EventPattern::Exact(event.clone())))
        .collect()
}

fn append_raises(
    raises: &[Box<str>],
    events: &mut Vec<EventPattern>,
    event_ids: &mut HashMap<EventPattern, EventId>,
    compiled: &mut Vec<EventId>,
) -> Result<u32, BuildError> {
    let start = compiled.len();
    for event in raises {
        compiled.push(intern_event(
            events,
            event_ids,
            EventPattern::Exact(event.clone()),
        )?);
    }
    u32::try_from(compiled.len() - start).map_err(|_| BuildError::TooManyActions)
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
    entries.extend(reverse.into_iter().rev().map(|state| Entry {
        state,
        take_initial: false,
    }));

    let mut cursor = target;
    while let DraftKind::Compound {
        initial: Some(initial),
    } = states[cursor.index()].kind
    {
        entries
            .last_mut()
            .expect("the transition target is always entered")
            .take_initial = true;
        entries.push(Entry {
            state: initial,
            take_initial: false,
        });
        cursor = initial;
    }

    u32::try_from(entries.len() - start).map_err(|_| BuildError::TooManyStates)
}

fn append_initial_descendants(mut state: StateId, states: &[StateNode], entries: &mut Vec<Entry>) {
    while let StateKind::Compound { initial, .. } = states[state.index()].kind {
        entries.push(Entry {
            state: initial,
            take_initial: matches!(states[initial.index()].kind, StateKind::Compound { .. }),
        });
        state = initial;
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
    #[error("final state {0:?} cannot have outgoing transitions")]
    FinalHasTransition(StateId),
    #[error("a definition cannot contain more than u32::MAX states")]
    TooManyStates,
    #[error("a definition cannot contain more than u32::MAX event names")]
    TooManyEvents,
    #[error("a definition cannot contain more than u32::MAX executable actions")]
    TooManyActions,
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
            second
        );
        assert_eq!(
            definition.compiled.state(first).transitions[1].target,
            first
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
