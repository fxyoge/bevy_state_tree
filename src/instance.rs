use std::{
    collections::VecDeque,
    marker::PhantomData,
    num::NonZeroUsize,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use bevy_asset::{AssetId, Handle};
use bevy_ecs::{component::Component, lifecycle::HookContext, world::DeferredWorld};

use crate::definition::{
    Entry, EventId, EventSet, LinkedDefinition, StateId, StateKind, Transition,
};
use crate::{StateTreeDefinition, StateTreeDomain, StateTreeEvents, plugin::StateTreeEventInbox};

static NEXT_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);

enum Trigger<'a> {
    Eventless,
    Internal(EventId),
    External(&'a EventSet),
}

struct HeadlessDomain<E>(PhantomData<fn() -> E>);

impl<E: StateTreeEvents> StateTreeDomain for HeadlessDomain<E> {
    type Event = E;
}

/// A headless execution of one compiled state tree.
///
/// This uses the same linked definition and macrostep engine as
/// [`StateTreeInstance`], without requiring a Bevy [`World`](bevy_ecs::world::World).
/// Construction performs all definition linking and reserves runtime storage.
pub struct StateTreeExecution<E: StateTreeEvents> {
    instance: StateTreeInstance<HeadlessDomain<E>>,
}

impl<E: StateTreeEvents> StateTreeExecution<E> {
    /// Links and initializes a definition for headless execution.
    pub fn new(definition: &StateTreeDefinition, microstep_limit: NonZeroUsize) -> Self {
        let linked = Arc::new(LinkedDefinition::new::<E>(Arc::clone(&definition.compiled)));
        let mut instance = StateTreeInstance::new(Handle::default());
        instance.initialize(linked, microstep_limit.get());
        Self { instance }
    }

    /// Returns the current execution status.
    pub fn status(&self) -> &InstanceStatus {
        self.instance.status()
    }

    /// Returns the active root-to-leaf authored state names.
    pub fn active_state_names(&self) -> impl Iterator<Item = &str> {
        self.instance.active_state_names()
    }

    /// Processes one external event through a complete macrostep.
    pub fn process_event(&mut self, event: &E, microstep_limit: NonZeroUsize) -> bool {
        self.instance
            .process_event(event.event_index(), event, microstep_limit.get())
    }
}

/// Current execution status of a state-tree instance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InstanceStatus {
    /// The definition asset has never been initialized.
    ///
    /// Events are not retained while an instance remains pending at the event
    /// processing boundary.
    Pending,
    /// The instance is stable and can receive events.
    Running,
    /// The instance entered a final child of the root state.
    Complete,
    /// Execution stopped because the definition could not stabilize.
    ///
    /// Faulted instances ignore subsequent events. Remove this component and
    /// insert a new [`StateTreeInstance`] to restart execution.
    Faulted(RuntimeFault),
}

/// A terminal state-tree execution fault.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeFault {
    /// Another transition set was enabled after the macrostep reached its limit.
    MicrostepLimitExceeded { limit: usize },
}

/// Mutable statechart state attached to one Bevy entity.
///
/// `D` identifies an independent runtime domain, allowing an application to
/// run simulation, UI, or dialogue trees in different schedules.
///
/// Initialization retains an [`Arc`] to the compiled definition, so removing
/// or replacing the source asset cannot invalidate a running instance. Asset
/// replacement is not currently applied to running instances; future hot
/// reload support can migrate that retained definition without reusing the
/// initial [`InstanceStatus::Pending`] state or replacing the event inbox.
#[derive(Component, Debug)]
#[component(on_remove = release_event_inbox::<D>)]
#[require(PendingStateTree<D>, StateTreeEventInbox<D>)]
pub struct StateTreeInstance<D: StateTreeDomain> {
    runtime_id: u64,
    definition: Handle<StateTreeDefinition>,
    linked: Option<Arc<LinkedDefinition>>,
    active: Vec<StateId>,
    internal_events: VecDeque<EventId>,
    status: InstanceStatus,
    marker: PhantomData<fn() -> D>,
}

fn release_event_inbox<D: StateTreeDomain>(mut world: DeferredWorld, context: HookContext) {
    let mut entity = world.entity_mut(context.entity);
    let Some(mut inbox) = entity.get_mut::<StateTreeEventInbox<D>>() else {
        return;
    };
    inbox.release_storage();
}

impl<D: StateTreeDomain> StateTreeInstance<D> {
    /// Creates an instance waiting for its definition asset.
    pub fn new(definition: Handle<StateTreeDefinition>) -> Self {
        Self {
            runtime_id: NEXT_INSTANCE_ID.fetch_add(1, Ordering::Relaxed),
            definition,
            linked: None,
            active: Vec::new(),
            internal_events: VecDeque::new(),
            status: InstanceStatus::Pending,
            marker: PhantomData,
        }
    }

    /// Returns the source definition handle.
    pub fn definition(&self) -> &Handle<StateTreeDefinition> {
        &self.definition
    }

    /// Returns the source definition asset ID.
    pub fn definition_id(&self) -> AssetId<StateTreeDefinition> {
        self.definition.id()
    }

    pub(crate) fn runtime_id(&self) -> u64 {
        self.runtime_id
    }

    /// Returns the current execution status.
    pub fn status(&self) -> &InstanceStatus {
        &self.status
    }

    /// Returns the active state IDs.
    pub fn active_states(&self) -> impl ExactSizeIterator<Item = StateId> + '_ {
        self.active.iter().copied()
    }

    /// Returns the active root-to-leaf authored state names.
    pub fn active_state_names(&self) -> impl Iterator<Item = &str> {
        let compiled = self
            .linked
            .as_deref()
            .map(|linked| linked.compiled.as_ref());
        self.active.iter().filter_map(move |state| {
            compiled.map(|definition| definition.state(*state).name.as_ref())
        })
    }

    pub(crate) fn initialize(&mut self, linked: Arc<LinkedDefinition>, microstep_limit: usize) {
        let compiled = &linked.compiled;
        self.linked = Some(Arc::clone(&linked));
        self.active.clear();
        self.active.reserve(compiled.max_depth);
        self.internal_events.clear();
        self.internal_events.reserve(compiled.max_depth);
        self.status = InstanceStatus::Running;
        for entry in &compiled.initial_entries {
            self.enter_state(compiled, *entry);
        }
        self.update_completion();
        if self.status == InstanceStatus::Running {
            self.stabilize(microstep_limit, 0);
        }
    }

    pub(crate) fn process_event<M: Send + Sync + 'static>(
        &mut self,
        event_index: usize,
        _payload: &M,
        microstep_limit: usize,
    ) -> bool {
        if self.status != InstanceStatus::Running {
            return false;
        }
        let Some(linked) = self.linked.as_ref() else {
            return false;
        };
        let Some(events) = linked.external_event(event_index) else {
            return false;
        };
        let Some(transition) = self.select_external_transition(events) else {
            return false;
        };

        self.take_transition(transition);
        self.update_completion();
        if self.status == InstanceStatus::Running {
            self.stabilize(microstep_limit, 1);
        }
        true
    }

    #[cfg(test)]
    fn process_authored_event(&mut self, name: &str, microstep_limit: usize) -> bool {
        if self.status != InstanceStatus::Running {
            return false;
        }
        let Some(event) = self
            .linked
            .as_ref()
            .and_then(|linked| linked.compiled.event(name))
        else {
            return false;
        };
        let Some(transition) = self.select_transition(Trigger::Internal(event)) else {
            return false;
        };
        self.take_transition(transition);
        self.update_completion();
        if self.status == InstanceStatus::Running {
            self.stabilize(microstep_limit, 1);
        }
        true
    }

    fn stabilize(&mut self, limit: usize, mut transitions_taken: usize) {
        loop {
            let transition = if let Some(transition) = self.select_transition(Trigger::Eventless) {
                transition
            } else {
                let Some(event) = self.internal_events.pop_front() else {
                    return;
                };
                let Some(transition) = self.select_transition(Trigger::Internal(event)) else {
                    continue;
                };
                transition
            };
            if transitions_taken == limit {
                self.status =
                    InstanceStatus::Faulted(RuntimeFault::MicrostepLimitExceeded { limit });
                return;
            }
            self.take_transition(transition);
            transitions_taken += 1;
            self.update_completion();
            if self.status != InstanceStatus::Running {
                return;
            }
        }
    }

    fn select_transition(&self, trigger: Trigger<'_>) -> Option<Transition> {
        let compiled = &self.linked.as_ref()?.compiled;
        self.active.iter().rev().find_map(|state| {
            compiled
                .state(*state)
                .transitions
                .iter()
                .find(|transition| match (transition.event, &trigger) {
                    (None, Trigger::Eventless) => true,
                    (Some(candidate), Trigger::Internal(emitted)) => {
                        compiled.internal_event_matches(candidate, *emitted)
                    }
                    (Some(event), Trigger::External(events)) => events.contains(event),
                    _ => false,
                })
                .copied()
        })
    }

    fn select_external_transition(&self, events: &EventSet) -> Option<Transition> {
        self.select_transition(Trigger::External(events))
    }

    fn take_transition(&mut self, transition: Transition) {
        let Some(compiled) = self
            .linked
            .as_ref()
            .map(|linked| Arc::clone(&linked.compiled))
        else {
            return;
        };
        for state in self.active[transition.exit_depth as usize..].iter().rev() {
            self.internal_events
                .extend(compiled.state(*state).exit_raises.iter().copied());
        }
        self.active.truncate(transition.exit_depth as usize);

        let raise_start = transition.raise_start as usize;
        let raise_end = raise_start + transition.raise_len as usize;
        self.internal_events.extend(
            compiled.transition_raises[raise_start..raise_end]
                .iter()
                .copied(),
        );

        let start = transition.entry_start as usize;
        let end = start + transition.entry_len as usize;
        for entry in &compiled.transition_entries[start..end] {
            self.enter_state(&compiled, *entry);
        }
        debug_assert!(
            self.active
                .iter()
                .enumerate()
                .all(|(depth, state)| compiled.state(*state).depth as usize == depth)
        );
    }

    fn enter_state(&mut self, compiled: &crate::definition::CompiledDefinition, entry: Entry) {
        self.active.push(entry.state);
        let state = compiled.state(entry.state);
        self.internal_events
            .extend(state.entry_raises.iter().copied());
        if entry.take_initial {
            self.internal_events
                .extend(state.initial_raises.iter().copied());
        }
    }

    fn update_completion(&mut self) {
        let Some(linked) = self.linked.as_ref() else {
            return;
        };
        let compiled = &linked.compiled;
        let Some(final_state) = self.active.last() else {
            return;
        };
        let final_node = compiled.state(*final_state);
        if final_node.kind != StateKind::Final {
            return;
        }
        let Some(parent) = final_node.parent else {
            return;
        };
        if parent == compiled.root {
            self.status = InstanceStatus::Complete;
            return;
        }
        let StateKind::Compound { completion, .. } = compiled.state(parent).kind else {
            return;
        };
        self.internal_events.push_back(completion);
    }
}

#[derive(Component, Debug)]
pub(crate) struct PendingStateTree<D: StateTreeDomain>(PhantomData<fn() -> D>);

impl<D: StateTreeDomain> Default for PendingStateTree<D> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

#[cfg(test)]
mod tests {
    use bevy_asset::Handle;

    use super::*;
    use crate::StateTreeBuilder;

    struct TestDomain;

    struct TestEvents;

    impl crate::StateTreeEvents for TestEvents {
        const AUTHORED_NAMES: &'static [&'static str] = &[];

        fn event_index(&self) -> usize {
            unreachable!()
        }
    }

    impl crate::StateTreeDomain for TestDomain {
        type Event = TestEvents;
    }

    fn instance(definition: &StateTreeDefinition, limit: usize) -> StateTreeInstance<TestDomain> {
        let mut instance = StateTreeInstance::new(Handle::default());
        instance.initialize(
            Arc::new(LinkedDefinition::new::<TestEvents>(Arc::clone(
                &definition.compiled,
            ))),
            limit,
        );
        instance
    }

    enum DescriptorEvents {
        Combat,
    }

    impl crate::StateTreeEvents for DescriptorEvents {
        const AUTHORED_NAMES: &'static [&'static str] = &["combat.damage"];

        fn event_index(&self) -> usize {
            match self {
                Self::Combat => 0,
            }
        }
    }

    #[test]
    fn linked_external_events_match_scxml_prefix_descriptors() {
        let mut builder = StateTreeBuilder::new("descriptors");
        let root = builder.compound_root("root").unwrap();
        let waiting = builder.atomic("waiting", root).unwrap();
        let done = builder.final_state("done", root).unwrap();
        builder.initial(root, waiting).unwrap();
        builder.event_descriptor(waiting, "combat", done).unwrap();
        let definition = builder.build().unwrap();
        let mut execution =
            StateTreeExecution::<DescriptorEvents>::new(&definition, NonZeroUsize::new(8).unwrap());

        assert!(execution.process_event(&DescriptorEvents::Combat, NonZeroUsize::new(8).unwrap()));
        assert_eq!(execution.status(), &InstanceStatus::Complete);
    }

    #[test]
    fn initializes_nested_compound_states_to_their_leaf() {
        let mut builder = StateTreeBuilder::new("nested");
        let root = builder.compound_root("root").unwrap();
        let parent = builder.compound("parent", root).unwrap();
        let leaf = builder.atomic("leaf", parent).unwrap();
        builder.initial(root, parent).unwrap();
        builder.initial(parent, leaf).unwrap();
        let definition = builder.build().unwrap();

        let instance = instance(&definition, 64);

        assert_eq!(instance.status(), &InstanceStatus::Running);
        assert_eq!(
            instance.active_state_names().collect::<Vec<_>>(),
            ["root", "parent", "leaf"]
        );
    }

    #[test]
    fn deepest_active_state_handles_an_event_first() {
        let mut builder = StateTreeBuilder::new("precedence");
        let root = builder.compound_root("root").unwrap();
        let parent = builder.compound("parent", root).unwrap();
        let leaf = builder.atomic("leaf", parent).unwrap();
        let parent_target = builder.atomic("parent-target", root).unwrap();
        let leaf_target = builder.atomic("leaf-target", root).unwrap();
        builder.initial(root, parent).unwrap();
        builder.initial(parent, leaf).unwrap();
        builder.transition(root, "advance", parent_target).unwrap();
        builder.transition(leaf, "advance", leaf_target).unwrap();
        let definition = builder.build().unwrap();
        let mut instance = instance(&definition, 64);

        instance.process_authored_event("advance", 64);

        assert_eq!(
            instance.active_state_names().collect::<Vec<_>>(),
            ["root", "leaf-target"]
        );
    }

    #[test]
    fn one_event_runs_eventless_transitions_until_stable() {
        let mut builder = StateTreeBuilder::new("stabilize");
        let root = builder.compound_root("root").unwrap();
        let idle = builder.atomic("idle", root).unwrap();
        let first = builder.atomic("first", root).unwrap();
        let second = builder.atomic("second", root).unwrap();
        let stable = builder.atomic("stable", root).unwrap();
        builder.initial(root, idle).unwrap();
        builder.transition(idle, "advance", first).unwrap();
        builder.eventless(first, second).unwrap();
        builder.eventless(second, stable).unwrap();
        let definition = builder.build().unwrap();
        let mut instance = instance(&definition, 3);

        instance.process_authored_event("advance", 3);

        assert_eq!(instance.status(), &InstanceStatus::Running);
        assert_eq!(
            instance.active_state_names().collect::<Vec<_>>(),
            ["root", "stable"]
        );
    }

    #[test]
    fn triggering_transition_counts_toward_the_microstep_limit() {
        let mut builder = StateTreeBuilder::new("event-limit");
        let root = builder.compound_root("root").unwrap();
        let idle = builder.atomic("idle", root).unwrap();
        let first = builder.atomic("first", root).unwrap();
        let stable = builder.atomic("stable", root).unwrap();
        builder.initial(root, idle).unwrap();
        builder.transition(idle, "advance", first).unwrap();
        builder.eventless(first, stable).unwrap();
        let definition = builder.build().unwrap();
        let mut instance = instance(&definition, 1);

        instance.process_authored_event("advance", 1);

        assert_eq!(
            instance.status(),
            &InstanceStatus::Faulted(RuntimeFault::MicrostepLimitExceeded { limit: 1 })
        );
        assert_eq!(
            instance.active_state_names().collect::<Vec<_>>(),
            ["root", "first"]
        );
    }

    #[test]
    fn one_triggering_transition_fits_a_limit_of_one() {
        let mut builder = StateTreeBuilder::new("single-event-step");
        let root = builder.compound_root("root").unwrap();
        let idle = builder.atomic("idle", root).unwrap();
        let stable = builder.atomic("stable", root).unwrap();
        builder.initial(root, idle).unwrap();
        builder.transition(idle, "advance", stable).unwrap();
        let definition = builder.build().unwrap();
        let mut instance = instance(&definition, 1);

        instance.process_authored_event("advance", 1);

        assert_eq!(instance.status(), &InstanceStatus::Running);
        assert_eq!(
            instance.active_state_names().collect::<Vec<_>>(),
            ["root", "stable"]
        );
    }

    #[test]
    fn initialization_receives_the_full_microstep_limit() {
        let mut builder = StateTreeBuilder::new("initial-limit");
        let root = builder.compound_root("root").unwrap();
        let first = builder.atomic("first", root).unwrap();
        let stable = builder.atomic("stable", root).unwrap();
        builder.initial(root, first).unwrap();
        builder.eventless(first, stable).unwrap();
        let definition = builder.build().unwrap();

        let instance = instance(&definition, 1);

        assert_eq!(instance.status(), &InstanceStatus::Running);
        assert_eq!(
            instance.active_state_names().collect::<Vec<_>>(),
            ["root", "stable"]
        );
    }

    #[test]
    fn faults_before_exceeding_the_microstep_limit() {
        let mut builder = StateTreeBuilder::new("cycle");
        let root = builder.compound_root("root").unwrap();
        let first = builder.atomic("first", root).unwrap();
        let second = builder.atomic("second", root).unwrap();
        builder.initial(root, first).unwrap();
        builder.eventless(first, second).unwrap();
        builder.eventless(second, first).unwrap();
        let definition = builder.build().unwrap();

        let instance = instance(&definition, 3);

        assert_eq!(
            instance.status(),
            &InstanceStatus::Faulted(RuntimeFault::MicrostepLimitExceeded { limit: 3 })
        );
        assert_eq!(
            instance.active_state_names().collect::<Vec<_>>(),
            ["root", "second"]
        );
    }

    #[test]
    fn entering_final_state_completes_the_instance() {
        let mut builder = StateTreeBuilder::new("complete");
        let root = builder.compound_root("root").unwrap();
        let running = builder.atomic("running", root).unwrap();
        let done = builder.final_state("done", root).unwrap();
        builder.initial(root, running).unwrap();
        builder.transition(running, "finish", done).unwrap();
        let definition = builder.build().unwrap();
        let mut instance = instance(&definition, 64);

        instance.process_authored_event("finish", 64);

        assert_eq!(instance.status(), &InstanceStatus::Complete);
        assert_eq!(
            instance.active_state_names().collect::<Vec<_>>(),
            ["root", "done"]
        );
    }

    #[test]
    fn nested_final_state_does_not_complete_the_root_chart() {
        let mut builder = StateTreeBuilder::new("nested-final");
        let root = builder.compound_root("root").unwrap();
        let nested = builder.compound("nested", root).unwrap();
        let running = builder.atomic("running", nested).unwrap();
        let done = builder.final_state("done", nested).unwrap();
        builder.initial(root, nested).unwrap();
        builder.initial(nested, running).unwrap();
        builder.transition(running, "finish", done).unwrap();
        let definition = builder.build().unwrap();
        let mut instance = instance(&definition, 64);

        instance.process_authored_event("finish", 64);

        assert_eq!(instance.status(), &InstanceStatus::Running);
        assert_eq!(
            instance.active_state_names().collect::<Vec<_>>(),
            ["root", "nested", "done"]
        );
    }

    #[test]
    fn nested_final_state_raises_parent_completion() {
        let mut builder = StateTreeBuilder::new("nested-completion");
        let root = builder.compound_root("root").unwrap();
        let nested = builder.compound("nested", root).unwrap();
        let running = builder.atomic("running", nested).unwrap();
        let nested_done = builder.final_state("nested-done", nested).unwrap();
        let complete = builder.final_state("complete", root).unwrap();
        builder.initial(root, nested).unwrap();
        builder.initial(nested, running).unwrap();
        builder.transition(running, "finish", nested_done).unwrap();
        builder
            .transition(nested, "done.state.nested", complete)
            .unwrap();
        let definition = builder.build().unwrap();
        let mut instance = instance(&definition, 64);

        instance.process_authored_event("finish", 64);

        assert_eq!(instance.status(), &InstanceStatus::Complete);
        assert_eq!(
            instance.active_state_names().collect::<Vec<_>>(),
            ["root", "complete"]
        );
    }

    #[test]
    fn initial_nested_final_state_raises_parent_completion() {
        let mut builder = StateTreeBuilder::new("initial-completion");
        let root = builder.compound_root("root").unwrap();
        let nested = builder.compound("nested", root).unwrap();
        let nested_done = builder.final_state("nested-done", nested).unwrap();
        let stable = builder.atomic("stable", root).unwrap();
        builder.initial(root, nested).unwrap();
        builder.initial(nested, nested_done).unwrap();
        builder
            .transition(nested, "done.state.nested", stable)
            .unwrap();
        let definition = builder.build().unwrap();

        let instance = instance(&definition, 64);

        assert_eq!(instance.status(), &InstanceStatus::Running);
        assert_eq!(
            instance.active_state_names().collect::<Vec<_>>(),
            ["root", "stable"]
        );
    }
}
