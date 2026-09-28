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
    pub kind: StateKind,
    pub transitions: Box<[Transition]>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Transition {
    pub event: Option<EventId>,
    pub target: StateId,
}

#[derive(Clone, Debug)]
pub(crate) struct CompiledDefinition {
    pub identity: u64,
    pub name: Box<str>,
    pub root: StateId,
    pub states: Box<[StateNode]>,
    events: HashMap<Box<str>, EventId>,
}

impl CompiledDefinition {
    pub fn event(&self, name: &str) -> Option<EventId> {
        self.events.get(name).copied()
    }

    pub fn state(&self, id: StateId) -> &StateNode {
        debug_assert_eq!(id.definition, self.identity);
        &self.states[id.index()]
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
    transitions: Vec<DraftTransition>,
}

#[derive(Clone, Debug)]
struct DraftTransition {
    event: Option<Box<str>>,
    target: StateId,
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
        self.add_transition(source, Some(event), target)
    }

    /// Adds an eventless external transition.
    pub fn eventless(&mut self, source: StateId, target: StateId) -> Result<&mut Self, BuildError> {
        self.add_transition(source, None, target)
    }

    /// Validates and compiles the definition.
    pub fn build(self) -> Result<StateTreeDefinition, BuildError> {
        if self.name.is_empty() {
            return Err(BuildError::EmptyDefinitionName);
        }
        let root = self.root.ok_or(BuildError::MissingRoot)?;

        let mut events = HashMap::<Box<str>, EventId>::new();
        let mut states = Vec::with_capacity(self.states.len());

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
                    let next =
                        u32::try_from(events.len()).map_err(|_| BuildError::TooManyEvents)?;
                    let completion = *events.entry(completion_name).or_insert(EventId(next));
                    StateKind::Compound {
                        initial: initial.ok_or(BuildError::MissingInitial(state_id))?,
                        completion,
                    }
                }
            };

            let mut transitions = Vec::with_capacity(draft.transitions.len());
            for transition in &draft.transitions {
                let event = match &transition.event {
                    Some(name) => {
                        let next =
                            u32::try_from(events.len()).map_err(|_| BuildError::TooManyEvents)?;
                        Some(*events.entry(name.clone()).or_insert(EventId(next)))
                    }
                    None => None,
                };
                transitions.push(Transition {
                    event,
                    target: transition.target,
                });
            }

            states.push(StateNode {
                name: draft.name.clone(),
                parent: draft.parent,
                kind,
                transitions: transitions.into_boxed_slice(),
            });
        }

        Ok(StateTreeDefinition {
            compiled: Arc::new(CompiledDefinition {
                identity: self.identity,
                name: self.name,
                root,
                states: states.into_boxed_slice(),
                events,
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
            transitions: Vec::new(),
        });
        self.names.insert(name, id);
        Ok(id)
    }

    fn add_transition(
        &mut self,
        source: StateId,
        event: Option<Box<str>>,
        target: StateId,
    ) -> Result<&mut Self, BuildError> {
        self.require_state(target)?;
        let state = self.require_state_mut(source)?;
        if matches!(state.kind, DraftKind::Final) {
            return Err(BuildError::FinalHasTransition(source));
        }
        state.transitions.push(DraftTransition { event, target });
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
