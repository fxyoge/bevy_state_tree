use std::{
    collections::VecDeque,
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use bevy_asset::{AssetId, Handle};
use bevy_ecs::{component::Component, lifecycle::HookContext, world::DeferredWorld};

use crate::definition::{CompiledDefinition, EventId, StateId, StateKind, Transition};
use crate::{StateTreeDefinition, plugin::StateTreeEventInbox};

static NEXT_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);

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
pub struct StateTreeInstance<D: Send + Sync + 'static> {
    runtime_id: u64,
    definition: Handle<StateTreeDefinition>,
    compiled: Option<Arc<CompiledDefinition>>,
    active: Vec<StateId>,
    internal_events: VecDeque<EventId>,
    status: InstanceStatus,
    marker: PhantomData<fn() -> D>,
}

fn release_event_inbox<D: Send + Sync + 'static>(mut world: DeferredWorld, context: HookContext) {
    let mut entity = world.entity_mut(context.entity);
    let Some(mut inbox) = entity.get_mut::<StateTreeEventInbox<D>>() else {
        return;
    };
    inbox.release_storage();
}

impl<D: Send + Sync + 'static> StateTreeInstance<D> {
    /// Creates an instance waiting for its definition asset.
    pub fn new(definition: Handle<StateTreeDefinition>) -> Self {
        Self {
            runtime_id: NEXT_INSTANCE_ID.fetch_add(1, Ordering::Relaxed),
            definition,
            compiled: None,
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
        let compiled = self.compiled.as_deref();
        self.active.iter().filter_map(move |state| {
            compiled.map(|definition| definition.state(*state).name.as_ref())
        })
    }

    pub(crate) fn initialize(&mut self, compiled: Arc<CompiledDefinition>, microstep_limit: usize) {
        self.compiled = Some(Arc::clone(&compiled));
        self.active.clear();
        self.internal_events.clear();
        self.status = InstanceStatus::Running;
        self.enter(compiled.root);
        self.update_completion();
        if self.status == InstanceStatus::Running {
            self.stabilize(microstep_limit, 0);
        }
    }

    pub(crate) fn process_event<M: Send + Sync + 'static>(
        &mut self,
        event_name: &str,
        _payload: &M,
        microstep_limit: usize,
    ) -> bool {
        if self.status != InstanceStatus::Running {
            return false;
        }
        let Some(compiled) = self.compiled.as_ref() else {
            return false;
        };
        let Some(event) = compiled.event(event_name) else {
            return false;
        };
        let Some(transition) = self.select_transition(Some(event)) else {
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
            let transition = if let Some(transition) = self.select_transition(None) {
                transition
            } else {
                let Some(event) = self.internal_events.pop_front() else {
                    return;
                };
                let Some(transition) = self.select_transition(Some(event)) else {
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

    fn select_transition(&self, event: Option<EventId>) -> Option<Transition> {
        let compiled = self.compiled.as_ref()?;
        self.active.iter().rev().find_map(|state| {
            compiled
                .state(*state)
                .transitions
                .iter()
                .find(|transition| transition.event == event)
                .copied()
        })
    }

    fn take_transition(&mut self, transition: Transition) {
        self.active.clear();
        self.enter(transition.target);
    }

    fn enter(&mut self, target: StateId) {
        let Some(compiled) = self.compiled.as_ref() else {
            return;
        };

        let mut ancestry = Vec::new();
        let mut cursor = Some(target);
        while let Some(state) = cursor {
            ancestry.push(state);
            cursor = compiled.state(state).parent;
        }
        self.active.extend(ancestry.into_iter().rev());

        let mut cursor = target;
        while let StateKind::Compound { initial, .. } = compiled.state(cursor).kind {
            self.active.push(initial);
            cursor = initial;
        }
    }

    fn update_completion(&mut self) {
        let Some(compiled) = self.compiled.as_ref() else {
            return;
        };
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
pub(crate) struct PendingStateTree<D: Send + Sync + 'static>(PhantomData<fn() -> D>);

impl<D: Send + Sync + 'static> Default for PendingStateTree<D> {
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

    fn instance(definition: &StateTreeDefinition, limit: usize) -> StateTreeInstance<TestDomain> {
        let mut instance = StateTreeInstance::new(Handle::default());
        instance.initialize(Arc::clone(&definition.compiled), limit);
        instance
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

        instance.process_event("advance", &(), 64);

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

        instance.process_event("advance", &(), 3);

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

        instance.process_event("advance", &(), 1);

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

        instance.process_event("advance", &(), 1);

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

        instance.process_event("finish", &(), 64);

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

        instance.process_event("finish", &(), 64);

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

        instance.process_event("finish", &(), 64);

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
