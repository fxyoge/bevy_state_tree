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
use bevy_ecs::{
    component::Component, entity::Entity, lifecycle::HookContext, world::DeferredWorld,
};

use crate::definition::{
    Action, CompiledDefinition, Entry, EventId, EventSet, ExpressionId, LinkedDefinition, StateId,
    StateKind, Transition,
};
use crate::effect::{
    HeadlessEffectSink, RejectEffects, RuntimeEffectSink, StateTreeEffectArgs,
    StateTreeEffectContext, StateTreeEffectSink,
};
use crate::{
    LuauValue, NoStateTreeTrace, StateTreeDefinition, StateTreeDomain, StateTreeEvents,
    StateTreeLinkError, StateTreeTrace, StateTreeTraceRecord, StateTreeTraceTrigger, luau,
    plugin::StateTreeEventInbox,
};

static NEXT_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);

enum Trigger<'a, E> {
    Eventless(Option<&'a str>, Option<&'a E>),
    Internal(EventId),
    External(&'a EventSet, &'a str, &'a E),
}

#[derive(Clone, Copy, Debug)]
struct SelectedTransition {
    source: StateId,
    index: usize,
    transition: Transition,
}

#[derive(Clone, Copy)]
enum SelectedStep {
    Single(SelectedTransition),
    Parallel,
}

#[derive(Debug, Default)]
struct ParallelRuntime {
    selected: Vec<SelectedTransition>,
    exited: Vec<StateId>,
    entries: Vec<Entry>,
    completed: Vec<StateId>,
}

struct ActionContext<'a, E> {
    event_name: Option<&'a str>,
    external_event: Option<&'a E>,
    entity: Option<Entity>,
}

impl<E> Clone for ActionContext<'_, E> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<E> Copy for ActionContext<'_, E> {}

impl<'a, E> Trigger<'a, E> {
    fn event_name(&self, compiled: &'a CompiledDefinition) -> Option<&'a str> {
        match self {
            Self::Eventless(name, _) => *name,
            Self::Internal(event) => compiled.event_name(*event),
            Self::External(_, name, _) => Some(name),
        }
    }

    fn external_event(&self) -> Option<&'a E> {
        match self {
            Self::Eventless(_, event) => *event,
            Self::External(_, _, event) => Some(*event),
            Self::Internal(_) => None,
        }
    }
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
    ///
    /// Linking fails when an authored event descriptor cannot match either the
    /// typed event protocol or an event produced internally by the definition.
    pub fn new(
        definition: &StateTreeDefinition,
        microstep_limit: NonZeroUsize,
    ) -> Result<Self, StateTreeLinkError> {
        Self::new_traced(definition, microstep_limit, &mut NoStateTreeTrace)
    }

    /// Links and initializes a definition while delivering effects to `sink`.
    pub fn new_with_sink<S: StateTreeEffectSink<E> + ?Sized>(
        definition: &StateTreeDefinition,
        microstep_limit: NonZeroUsize,
        sink: &mut S,
    ) -> Result<Self, StateTreeLinkError> {
        Self::new_traced_with_sink(definition, microstep_limit, sink, &mut NoStateTreeTrace)
    }

    /// Links and initializes a definition while writing its semantic trace.
    pub fn new_traced<T: StateTreeTrace + ?Sized>(
        definition: &StateTreeDefinition,
        microstep_limit: NonZeroUsize,
        trace: &mut T,
    ) -> Result<Self, StateTreeLinkError> {
        let linked = Arc::new(LinkedDefinition::<HeadlessDomain<E>>::new(
            Arc::clone(&definition.compiled),
            None,
        )?);
        let mut instance = StateTreeInstance::new(Handle::default());
        instance.initialize_traced(
            linked,
            microstep_limit.get(),
            None,
            &mut RejectEffects,
            trace,
        );
        Ok(Self { instance })
    }

    /// Links and initializes a definition while delivering effects and tracing execution.
    pub fn new_traced_with_sink<S: StateTreeEffectSink<E> + ?Sized, T: StateTreeTrace + ?Sized>(
        definition: &StateTreeDefinition,
        microstep_limit: NonZeroUsize,
        sink: &mut S,
        trace: &mut T,
    ) -> Result<Self, StateTreeLinkError> {
        let linked = Arc::new(LinkedDefinition::<HeadlessDomain<E>>::new(
            Arc::clone(&definition.compiled),
            None,
        )?);
        let mut instance = StateTreeInstance::new(Handle::default());
        instance.initialize_traced(
            linked,
            microstep_limit.get(),
            None,
            &mut HeadlessEffectSink { sink },
            trace,
        );
        Ok(Self { instance })
    }

    /// Returns the current execution status.
    pub fn status(&self) -> &InstanceStatus {
        self.instance.status()
    }

    /// Returns the active authored state names in canonical definition order.
    pub fn active_state_names(&self) -> impl Iterator<Item = &str> {
        self.instance.active_state_names()
    }

    /// Returns one instance-local Luau datamodel value by authored id.
    pub fn data(&self, name: &str) -> Option<&LuauValue> {
        self.instance.data(name)
    }

    /// Processes one external event through a complete macrostep.
    pub fn process_event(&mut self, event: &E, microstep_limit: NonZeroUsize) -> bool {
        self.process_event_traced(event, microstep_limit, &mut NoStateTreeTrace)
    }

    /// Processes one external event while delivering effects to `sink`.
    pub fn process_event_with_sink<S: StateTreeEffectSink<E> + ?Sized>(
        &mut self,
        event: &E,
        microstep_limit: NonZeroUsize,
        sink: &mut S,
    ) -> bool {
        self.instance.process_event_traced(
            event,
            microstep_limit.get(),
            None,
            &mut HeadlessEffectSink { sink },
            &mut NoStateTreeTrace,
        )
    }

    /// Processes one external event while writing its complete semantic trace.
    pub fn process_event_traced<T: StateTreeTrace + ?Sized>(
        &mut self,
        event: &E,
        microstep_limit: NonZeroUsize,
        trace: &mut T,
    ) -> bool {
        self.instance.process_event_traced(
            event,
            microstep_limit.get(),
            None,
            &mut RejectEffects,
            trace,
        )
    }

    /// Processes one external event while delivering effects and tracing execution.
    pub fn process_event_traced_with_sink<
        S: StateTreeEffectSink<E> + ?Sized,
        T: StateTreeTrace + ?Sized,
    >(
        &mut self,
        event: &E,
        microstep_limit: NonZeroUsize,
        sink: &mut S,
        trace: &mut T,
    ) -> bool {
        self.instance.process_event_traced(
            event,
            microstep_limit.get(),
            None,
            &mut HeadlessEffectSink { sink },
            trace,
        )
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
    ///
    /// The root and reached final state remain active for inspection. Completion
    /// is not an implicit teardown, so the final state's exit actions do not run.
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
    /// A Luau datamodel expression failed while the instance was executing.
    Datamodel {
        operation: DatamodelOperation,
        span_start: usize,
        message: Box<str>,
    },
    /// A typed effect could not be constructed or published.
    Effect {
        effect: Box<str>,
        argument: Option<Box<str>>,
        span_start: usize,
        message: Box<str>,
    },
    /// A typed external event could not be projected into Luau data.
    EventDataProjection { event: Box<str>, message: Box<str> },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DatamodelOperation {
    Initialize,
    Condition,
    Assign,
    EffectArgument,
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
    linked: Option<Arc<LinkedDefinition<D>>>,
    active: Vec<StateId>,
    parallel: Option<Box<ParallelRuntime>>,
    internal_events: VecDeque<EventId>,
    data: Vec<LuauValue>,
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
            parallel: None,
            internal_events: VecDeque::new(),
            data: Vec::new(),
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

    /// Returns the active authored state names in canonical definition order.
    pub fn active_state_names(&self) -> impl Iterator<Item = &str> {
        let compiled = self
            .linked
            .as_deref()
            .map(|linked| linked.compiled.as_ref());
        self.active.iter().filter_map(move |state| {
            compiled.map(|definition| definition.state(*state).name.as_ref())
        })
    }

    /// Returns one instance-local Luau datamodel value by authored id.
    pub fn data(&self, name: &str) -> Option<&LuauValue> {
        let datamodel = self.linked.as_ref()?.compiled.datamodel.as_ref()?;
        datamodel
            .names
            .iter()
            .position(|candidate| candidate.as_ref() == name)
            .and_then(|index| self.data.get(index))
    }

    pub(crate) fn initialize<S: RuntimeEffectSink<D> + ?Sized>(
        &mut self,
        linked: Arc<LinkedDefinition<D>>,
        microstep_limit: usize,
        entity: Option<Entity>,
        effects: &mut S,
    ) {
        self.initialize_traced(
            linked,
            microstep_limit,
            entity,
            effects,
            &mut NoStateTreeTrace,
        );
    }

    fn initialize_traced<S: RuntimeEffectSink<D> + ?Sized, T: StateTreeTrace + ?Sized>(
        &mut self,
        linked: Arc<LinkedDefinition<D>>,
        microstep_limit: usize,
        entity: Option<Entity>,
        effects: &mut S,
        trace: &mut T,
    ) {
        let compiled = &linked.compiled;
        trace.record(StateTreeTraceRecord::MacrostepStarted {
            trigger: StateTreeTraceTrigger::Initialization,
        });
        self.linked = Some(Arc::clone(&linked));
        self.active.clear();
        self.active.reserve(compiled.max_active);
        self.parallel = compiled.has_parallel.then(|| {
            let mut runtime = Box::<ParallelRuntime>::default();
            runtime.selected.reserve(compiled.max_active);
            runtime.exited.reserve(compiled.max_active);
            runtime.entries.reserve(compiled.max_active);
            runtime.completed.reserve(compiled.max_active);
            runtime
        });
        self.internal_events.clear();
        self.internal_events
            .reserve(compiled.max_depth.max(compiled.max_active));
        self.data.clear();
        self.status = InstanceStatus::Running;
        if let Err(fault) = self.initialize_datamodel(compiled, trace) {
            self.status = InstanceStatus::Faulted(fault);
            self.finish_macrostep(compiled, trace);
            return;
        }
        for entry in &compiled.initial_entries {
            if let Err(fault) = self.enter_state(
                compiled,
                *entry,
                ActionContext {
                    event_name: None,
                    external_event: None,
                    entity,
                },
                effects,
                trace,
            ) {
                self.status = InstanceStatus::Faulted(fault);
                self.finish_macrostep(compiled, trace);
                return;
            }
        }
        self.update_completion(trace);
        if self.status == InstanceStatus::Running {
            self.stabilize(
                microstep_limit,
                0,
                ActionContext {
                    event_name: None,
                    external_event: None,
                    entity,
                },
                effects,
                trace,
            );
        }
        self.finish_macrostep(compiled, trace);
    }

    pub(crate) fn process_event<S: RuntimeEffectSink<D> + ?Sized>(
        &mut self,
        event: &D::Event,
        microstep_limit: usize,
        entity: Option<Entity>,
        effects: &mut S,
    ) -> bool {
        self.process_event_traced(
            event,
            microstep_limit,
            entity,
            effects,
            &mut NoStateTreeTrace,
        )
    }

    fn process_event_traced<S: RuntimeEffectSink<D> + ?Sized, T: StateTreeTrace + ?Sized>(
        &mut self,
        event: &D::Event,
        microstep_limit: usize,
        entity: Option<Entity>,
        effects: &mut S,
        trace: &mut T,
    ) -> bool {
        if self.status != InstanceStatus::Running {
            return false;
        }
        let event_name = event.authored_name();
        trace.record(StateTreeTraceRecord::MacrostepStarted {
            trigger: StateTreeTraceTrigger::ExternalEvent(event_name),
        });
        let selected = if self.parallel.is_some() {
            let Some(linked) = self.linked.as_ref().map(Arc::clone) else {
                return false;
            };
            let Some(events) = linked.external_event(event.event_index()) else {
                return false;
            };
            self.select_step(Trigger::External(events, event_name, event))
        } else {
            let Some(linked) = self.linked.as_ref() else {
                return false;
            };
            let Some(events) = linked.external_event(event.event_index()) else {
                return false;
            };
            self.select_transition(Trigger::External(events, event_name, event))
                .map(|selected| selected.map(SelectedStep::Single))
        };
        let transition = match selected {
            Ok(Some(transition)) => transition,
            Ok(None) => {
                self.finish_current_macrostep(trace);
                return false;
            }
            Err(fault) => {
                self.status = InstanceStatus::Faulted(fault);
                self.finish_current_macrostep(trace);
                return true;
            }
        };
        let configuration_changed = self.selected_step_changes_configuration(transition);

        self.trace_selected_step(1, transition, trace);
        if let Err(fault) = self.take_selected_step(
            transition,
            ActionContext {
                event_name: Some(event_name),
                external_event: Some(event),
                entity,
            },
            effects,
            trace,
        ) {
            self.status = InstanceStatus::Faulted(fault);
            self.finish_current_macrostep(trace);
            return true;
        }
        if configuration_changed {
            self.update_completion(trace);
        }
        if self.status == InstanceStatus::Running {
            self.stabilize(
                microstep_limit,
                1,
                ActionContext {
                    event_name: Some(event_name),
                    external_event: Some(event),
                    entity,
                },
                effects,
                trace,
            );
        }
        self.finish_current_macrostep(trace);
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
        let transition = match self.select_step(Trigger::Internal(event)) {
            Ok(Some(transition)) => transition,
            Ok(None) => return false,
            Err(fault) => {
                self.status = InstanceStatus::Faulted(fault);
                return true;
            }
        };
        let name = self
            .linked
            .as_ref()
            .and_then(|linked| linked.compiled.event_name(event))
            .map(str::to_owned);
        let configuration_changed = self.selected_step_changes_configuration(transition);
        self.trace_selected_step(1, transition, &mut NoStateTreeTrace);
        if let Err(fault) = self.take_selected_step(
            transition,
            ActionContext {
                event_name: name.as_deref(),
                external_event: None,
                entity: None,
            },
            &mut RejectEffects,
            &mut NoStateTreeTrace,
        ) {
            self.status = InstanceStatus::Faulted(fault);
            return true;
        }
        if configuration_changed {
            self.update_completion(&mut NoStateTreeTrace);
        }
        if self.status == InstanceStatus::Running {
            self.stabilize(
                microstep_limit,
                1,
                ActionContext {
                    event_name: name.as_deref(),
                    external_event: None,
                    entity: None,
                },
                &mut RejectEffects,
                &mut NoStateTreeTrace,
            );
        }
        true
    }

    fn stabilize<S: RuntimeEffectSink<D> + ?Sized, T: StateTreeTrace + ?Sized>(
        &mut self,
        limit: usize,
        mut transitions_taken: usize,
        context: ActionContext<'_, D::Event>,
        effects: &mut S,
        trace: &mut T,
    ) {
        let compiled = self
            .linked
            .as_ref()
            .map(|linked| Arc::clone(&linked.compiled));
        let mut current_event = context.event_name;
        let mut external_event = context.external_event;
        loop {
            let transition =
                match self.select_step(Trigger::Eventless(current_event, external_event)) {
                    Ok(Some(transition)) => transition,
                    Ok(None) => {
                        let Some(event) = self.internal_events.pop_front() else {
                            return;
                        };
                        current_event = compiled
                            .as_ref()
                            .and_then(|compiled| compiled.event_name(event));
                        external_event = None;
                        if let Some(event_name) = current_event {
                            trace.record(StateTreeTraceRecord::InternalEventConsumed {
                                event: event_name,
                            });
                        }
                        match self.select_step(Trigger::Internal(event)) {
                            Ok(Some(transition)) => transition,
                            Ok(None) => continue,
                            Err(fault) => {
                                self.status = InstanceStatus::Faulted(fault);
                                return;
                            }
                        }
                    }
                    Err(fault) => {
                        self.status = InstanceStatus::Faulted(fault);
                        return;
                    }
                };
            if transitions_taken == limit {
                self.status =
                    InstanceStatus::Faulted(RuntimeFault::MicrostepLimitExceeded { limit });
                return;
            }
            let configuration_changed = self.selected_step_changes_configuration(transition);
            self.trace_selected_step(transitions_taken + 1, transition, trace);
            if let Err(fault) = self.take_selected_step(
                transition,
                ActionContext {
                    event_name: current_event,
                    external_event,
                    entity: context.entity,
                },
                effects,
                trace,
            ) {
                self.status = InstanceStatus::Faulted(fault);
                return;
            }
            transitions_taken += 1;
            if configuration_changed {
                self.update_completion(trace);
            }
            if self.status != InstanceStatus::Running {
                return;
            }
        }
    }

    fn select_transition(
        &self,
        trigger: Trigger<'_, D::Event>,
    ) -> Result<Option<SelectedTransition>, RuntimeFault> {
        let Some(compiled) = self.linked.as_ref().map(|linked| &linked.compiled) else {
            return Ok(None);
        };
        for state in self.active.iter().rev() {
            for (index, transition) in compiled.state(*state).transitions.iter().enumerate() {
                let matches = match (transition.event, &trigger) {
                    (None, Trigger::Eventless(_, _)) => true,
                    (Some(candidate), Trigger::Internal(emitted)) => {
                        compiled.internal_event_matches(candidate, *emitted)
                    }
                    (Some(event), Trigger::External(events, _, _)) => events.contains(event),
                    _ => false,
                };
                if !matches {
                    continue;
                }
                if let Some(condition) = transition.condition
                    && !self.evaluate_condition(
                        compiled,
                        condition,
                        trigger.event_name(compiled),
                        trigger.external_event(),
                    )?
                {
                    continue;
                }
                return Ok(Some(SelectedTransition {
                    source: *state,
                    index,
                    transition: *transition,
                }));
            }
        }
        Ok(None)
    }

    fn select_step(
        &mut self,
        trigger: Trigger<'_, D::Event>,
    ) -> Result<Option<SelectedStep>, RuntimeFault> {
        if self.parallel.is_none() {
            return self
                .select_transition(trigger)
                .map(|selected| selected.map(SelectedStep::Single));
        }

        let mut runtime = self.parallel.take().expect("parallel runtime exists");
        let result = self.select_parallel_transition_set(&trigger, &mut runtime.selected);
        let selected =
            result.map(|()| (!runtime.selected.is_empty()).then_some(SelectedStep::Parallel));
        self.parallel = Some(runtime);
        selected
    }

    fn selected_step_changes_configuration(&self, selected: SelectedStep) -> bool {
        match selected {
            SelectedStep::Single(selected) => selected.transition.target.is_some(),
            SelectedStep::Parallel => self
                .parallel
                .as_ref()
                .expect("parallel runtime exists")
                .selected
                .iter()
                .any(|selected| selected.transition.target.is_some()),
        }
    }

    fn select_parallel_transition_set(
        &self,
        trigger: &Trigger<'_, D::Event>,
        selected: &mut Vec<SelectedTransition>,
    ) -> Result<(), RuntimeFault> {
        selected.clear();
        let compiled = &self
            .linked
            .as_ref()
            .expect("initialized instance has a definition")
            .compiled;

        for leaf in self.active.iter().copied().filter(|state| {
            matches!(
                compiled.state(*state).kind,
                StateKind::Atomic | StateKind::Final
            )
        }) {
            let mut cursor = Some(leaf);
            let mut candidate = None;
            while let Some(state) = cursor {
                for (index, transition) in compiled.state(state).transitions.iter().enumerate() {
                    let matches = match (transition.event, trigger) {
                        (None, Trigger::Eventless(_, _)) => true,
                        (Some(candidate), Trigger::Internal(emitted)) => {
                            compiled.internal_event_matches(candidate, *emitted)
                        }
                        (Some(event), Trigger::External(events, _, _)) => events.contains(event),
                        _ => false,
                    };
                    if !matches {
                        continue;
                    }
                    if let Some(condition) = transition.condition
                        && !self.evaluate_condition(
                            compiled,
                            condition,
                            trigger.event_name(compiled),
                            trigger.external_event(),
                        )?
                    {
                        continue;
                    }
                    candidate = Some(SelectedTransition {
                        source: state,
                        index,
                        transition: *transition,
                    });
                    break;
                }
                if candidate.is_some() {
                    break;
                }
                cursor = compiled.state(state).parent;
            }

            let Some(candidate) = candidate else {
                continue;
            };
            if selected.iter().any(|existing| {
                existing.source == candidate.source && existing.index == candidate.index
            }) {
                continue;
            }

            let mut accepted = true;
            let mut index = 0;
            while index < selected.len() {
                if !transitions_conflict(compiled, selected[index], candidate) {
                    index += 1;
                    continue;
                }
                if is_descendant(compiled, candidate.source, selected[index].source) {
                    selected.remove(index);
                } else {
                    accepted = false;
                    break;
                }
            }
            if accepted {
                selected.push(candidate);
            }
        }
        selected.sort_unstable_by_key(|candidate| compiled.state(candidate.source).document_order);
        Ok(())
    }

    fn take_selected_step<S: RuntimeEffectSink<D> + ?Sized, T: StateTreeTrace + ?Sized>(
        &mut self,
        selected: SelectedStep,
        context: ActionContext<'_, D::Event>,
        effects: &mut S,
        trace: &mut T,
    ) -> Result<(), RuntimeFault> {
        match selected {
            SelectedStep::Single(selected) => {
                self.take_transition(selected, context, effects, trace)
            }
            SelectedStep::Parallel => {
                let mut runtime = self.parallel.take().expect("parallel runtime exists");
                let result =
                    self.take_parallel_transition_set(&mut runtime, context, effects, trace);
                self.parallel = Some(runtime);
                result
            }
        }
    }

    fn take_parallel_transition_set<
        S: RuntimeEffectSink<D> + ?Sized,
        T: StateTreeTrace + ?Sized,
    >(
        &mut self,
        runtime: &mut ParallelRuntime,
        context: ActionContext<'_, D::Event>,
        effects: &mut S,
        trace: &mut T,
    ) -> Result<(), RuntimeFault> {
        let compiled = Arc::clone(
            &self
                .linked
                .as_ref()
                .expect("initialized instance has a definition")
                .compiled,
        );
        runtime.exited.clear();
        for selected in &runtime.selected {
            if selected.transition.target.is_none() {
                continue;
            }
            for state in &self.active {
                if exits_domain(&compiled, *state, selected.transition.domain)
                    && !runtime.exited.contains(state)
                {
                    runtime.exited.push(*state);
                }
            }
        }
        runtime.exited.sort_unstable_by(|left, right| {
            compiled
                .state(*right)
                .depth
                .cmp(&compiled.state(*left).depth)
                .then_with(|| {
                    compiled
                        .state(*left)
                        .document_order
                        .cmp(&compiled.state(*right).document_order)
                })
        });

        for state in &runtime.exited {
            trace.record(StateTreeTraceRecord::StateExitStarted {
                state: &compiled.state(*state).name,
            });
            self.execute_actions(
                &compiled,
                &compiled.state(*state).exit_actions,
                context,
                effects,
                trace,
            )?;
        }
        for state in &runtime.exited {
            trace.record(StateTreeTraceRecord::StateExited {
                state: &compiled.state(*state).name,
            });
        }
        self.active.retain(|state| !runtime.exited.contains(state));
        runtime
            .completed
            .retain(|state| !runtime.exited.contains(state));

        for selected in &runtime.selected {
            let start = selected.transition.action_start as usize;
            let end = start + selected.transition.action_len as usize;
            self.execute_actions(
                &compiled,
                &compiled.transition_actions[start..end],
                context,
                effects,
                trace,
            )?;
        }

        runtime.entries.clear();
        for selected in &runtime.selected {
            let start = selected.transition.entry_start as usize;
            let end = start + selected.transition.entry_len as usize;
            for entry in &compiled.transition_entries[start..end] {
                if self.active.contains(&entry.state) {
                    continue;
                }
                if let Some(existing) = runtime
                    .entries
                    .iter_mut()
                    .find(|existing| existing.state == entry.state)
                {
                    existing.take_initial |= entry.take_initial;
                } else {
                    runtime.entries.push(*entry);
                }
            }
        }
        runtime
            .entries
            .sort_unstable_by_key(|entry| compiled.state(entry.state).document_order);
        for index in 0..runtime.entries.len() {
            let entry = runtime.entries[index];
            if let Err(fault) = self.enter_state(&compiled, entry, context, effects, trace) {
                self.active
                    .sort_unstable_by_key(|state| compiled.state(*state).document_order);
                return Err(fault);
            }
        }
        self.active
            .sort_unstable_by_key(|state| compiled.state(*state).document_order);
        Ok(())
    }

    fn take_transition<S: RuntimeEffectSink<D> + ?Sized, T: StateTreeTrace + ?Sized>(
        &mut self,
        selected: SelectedTransition,
        context: ActionContext<'_, D::Event>,
        effects: &mut S,
        trace: &mut T,
    ) -> Result<(), RuntimeFault> {
        let Some(compiled) = self
            .linked
            .as_ref()
            .map(|linked| Arc::clone(&linked.compiled))
        else {
            return Ok(());
        };
        let transition = selected.transition;
        if transition.target.is_none() {
            let action_start = transition.action_start as usize;
            let action_end = action_start + transition.action_len as usize;
            return self.execute_actions(
                &compiled,
                &compiled.transition_actions[action_start..action_end],
                context,
                effects,
                trace,
            );
        }
        let exit_depth = transition.exit_depth as usize;
        for index in (exit_depth..self.active.len()).rev() {
            let state = self.active[index];
            trace.record(StateTreeTraceRecord::StateExitStarted {
                state: &compiled.state(state).name,
            });
            self.execute_actions(
                &compiled,
                &compiled.state(state).exit_actions,
                context,
                effects,
                trace,
            )?;
        }
        for index in (exit_depth..self.active.len()).rev() {
            let state = self.active[index];
            trace.record(StateTreeTraceRecord::StateExited {
                state: &compiled.state(state).name,
            });
        }
        self.active.truncate(exit_depth);

        let action_start = transition.action_start as usize;
        let action_end = action_start + transition.action_len as usize;
        self.execute_actions(
            &compiled,
            &compiled.transition_actions[action_start..action_end],
            context,
            effects,
            trace,
        )?;

        let start = transition.entry_start as usize;
        let end = start + transition.entry_len as usize;
        for entry in &compiled.transition_entries[start..end] {
            self.enter_state(&compiled, *entry, context, effects, trace)?;
        }
        debug_assert!(
            self.active
                .iter()
                .enumerate()
                .all(|(depth, state)| compiled.state(*state).depth as usize == depth)
        );
        Ok(())
    }

    fn trace_transition_set<T: StateTreeTrace + ?Sized>(
        &self,
        microstep: usize,
        selected: SelectedTransition,
        trace: &mut T,
    ) {
        let Some(compiled) = self.linked.as_ref().map(|linked| linked.compiled.as_ref()) else {
            return;
        };
        trace.record(StateTreeTraceRecord::MicrostepStarted { index: microstep });
        trace.record(StateTreeTraceRecord::TransitionSelected {
            source: &compiled.state(selected.source).name,
        });
        if let Some(target) = selected.transition.target {
            trace.record(StateTreeTraceRecord::TransitionTarget {
                state: &compiled.state(target).name,
            });
        } else {
            trace.record(StateTreeTraceRecord::TransitionTargetless);
        }
    }

    fn trace_selected_step<T: StateTreeTrace + ?Sized>(
        &self,
        microstep: usize,
        selected: SelectedStep,
        trace: &mut T,
    ) {
        match selected {
            SelectedStep::Single(selected) => {
                self.trace_transition_set(microstep, selected, trace);
            }
            SelectedStep::Parallel => {
                let Some(compiled) = self.linked.as_ref().map(|linked| linked.compiled.as_ref())
                else {
                    return;
                };
                let runtime = self.parallel.as_ref().expect("parallel runtime exists");
                trace.record(StateTreeTraceRecord::MicrostepStarted { index: microstep });
                for selected in &runtime.selected {
                    trace.record(StateTreeTraceRecord::TransitionSelected {
                        source: &compiled.state(selected.source).name,
                    });
                    if let Some(target) = selected.transition.target {
                        trace.record(StateTreeTraceRecord::TransitionTarget {
                            state: &compiled.state(target).name,
                        });
                    } else {
                        trace.record(StateTreeTraceRecord::TransitionTargetless);
                    }
                }
            }
        }
    }

    fn enter_state<S: RuntimeEffectSink<D> + ?Sized, T: StateTreeTrace + ?Sized>(
        &mut self,
        compiled: &CompiledDefinition,
        entry: Entry,
        context: ActionContext<'_, D::Event>,
        effects: &mut S,
        trace: &mut T,
    ) -> Result<(), RuntimeFault> {
        self.active.push(entry.state);
        let state = compiled.state(entry.state);
        trace.record(StateTreeTraceRecord::StateEntered { state: &state.name });
        self.execute_actions(compiled, &state.entry_actions, context, effects, trace)?;
        if entry.take_initial {
            self.execute_actions(compiled, &state.initial_actions, context, effects, trace)?;
        }
        Ok(())
    }

    fn initialize_datamodel<T: StateTreeTrace + ?Sized>(
        &mut self,
        compiled: &CompiledDefinition,
        trace: &mut T,
    ) -> Result<(), RuntimeFault> {
        let Some(datamodel) = &compiled.datamodel else {
            return Ok(());
        };
        self.data.resize(datamodel.names.len(), LuauValue::Nil);
        for (index, initializer) in datamodel.initializers.iter().enumerate() {
            let Some(initializer) = initializer else {
                continue;
            };
            self.data[index] = self.evaluate_expression(
                compiled,
                *initializer,
                None,
                None,
                DatamodelOperation::Initialize,
            )?;
            trace.record(StateTreeTraceRecord::DataAssigned {
                data: &datamodel.names[index],
                value: &self.data[index],
            });
        }
        Ok(())
    }

    fn evaluate_condition(
        &self,
        compiled: &CompiledDefinition,
        expression: ExpressionId,
        event_name: Option<&str>,
        external_event: Option<&D::Event>,
    ) -> Result<bool, RuntimeFault> {
        match self.evaluate_expression(
            compiled,
            expression,
            event_name,
            external_event,
            DatamodelOperation::Condition,
        )? {
            LuauValue::Boolean(value) => Ok(value),
            _ => unreachable!("condition expressions are compiled to booleans"),
        }
    }

    fn evaluate_expression(
        &self,
        compiled: &CompiledDefinition,
        expression: ExpressionId,
        event_name: Option<&str>,
        external_event: Option<&D::Event>,
        operation: DatamodelOperation,
    ) -> Result<LuauValue, RuntimeFault> {
        let datamodel = compiled
            .datamodel
            .as_ref()
            .expect("compiled Luau expressions require a datamodel");
        let expression = &datamodel.expressions[expression.index()];
        let event_data = if expression.reads_event_data {
            external_event
                .map(StateTreeEvents::event_data)
                .transpose()
                .map_err(|error| RuntimeFault::EventDataProjection {
                    event: event_name.unwrap_or_default().into(),
                    message: error.to_string().into(),
                })?
                .flatten()
        } else {
            None
        };
        luau::evaluate(
            expression,
            &datamodel.names,
            &self.data,
            event_name,
            event_data.as_ref(),
        )
        .map_err(|error| RuntimeFault::Datamodel {
            operation,
            span_start: expression.span_start,
            message: error.to_string().into(),
        })
    }

    fn execute_actions<S: RuntimeEffectSink<D> + ?Sized, T: StateTreeTrace + ?Sized>(
        &mut self,
        compiled: &CompiledDefinition,
        actions: &[Action],
        context: ActionContext<'_, D::Event>,
        effects: &mut S,
        trace: &mut T,
    ) -> Result<(), RuntimeFault> {
        for action in actions {
            match *action {
                Action::Raise(event) => {
                    self.internal_events.push_back(event);
                    if let Some(event) = compiled.event_name(event) {
                        trace.record(StateTreeTraceRecord::InternalEventRaised { event });
                    }
                }
                Action::Assign { data, expression } => {
                    let value = self.evaluate_expression(
                        compiled,
                        expression,
                        context.event_name,
                        context.external_event,
                        DatamodelOperation::Assign,
                    )?;
                    self.data[data.index()] = value;
                    let datamodel = compiled
                        .datamodel
                        .as_ref()
                        .expect("compiled assignments require a datamodel");
                    trace.record(StateTreeTraceRecord::DataAssigned {
                        data: &datamodel.names[data.index()],
                        value: &self.data[data.index()],
                    });
                }
                Action::Effect {
                    effect,
                    argument_start,
                    argument_len,
                    span_start,
                } => {
                    let start = argument_start as usize;
                    let end = start + argument_len as usize;
                    let mut values = Vec::with_capacity(argument_len as usize);
                    for argument in &compiled.effect_arguments[start..end] {
                        let value = self.evaluate_expression(
                            compiled,
                            argument.expression,
                            context.event_name,
                            context.external_event,
                            DatamodelOperation::EffectArgument,
                        )?;
                        values.push((argument.name.clone(), value));
                    }
                    let (data_names, data) = compiled
                        .datamodel
                        .as_ref()
                        .map_or((&[][..], &[][..]), |datamodel| {
                            (datamodel.names.as_ref(), self.data.as_slice())
                        });
                    let handler = self
                        .linked
                        .as_ref()
                        .and_then(|linked| linked.effect(effect))
                        .cloned();
                    let name = compiled.effect_name(effect);
                    let context = StateTreeEffectContext {
                        entity: context.entity,
                        event_name: context.event_name,
                        external_event: context.external_event,
                        data_names,
                        data,
                    };
                    if let Err(error) = effects.emit(
                        name,
                        handler.as_ref(),
                        context,
                        StateTreeEffectArgs::new(values),
                        span_start as usize,
                    ) {
                        return Err(RuntimeFault::Effect {
                            effect: name.into(),
                            argument: error.argument_name().map(Into::into),
                            span_start: span_start as usize,
                            message: error.message().into(),
                        });
                    }
                    trace.record(StateTreeTraceRecord::EffectEmitted { effect: name });
                }
            }
        }
        Ok(())
    }

    fn update_completion<T: StateTreeTrace + ?Sized>(&mut self, trace: &mut T) {
        if self.parallel.is_some() {
            let mut runtime = self.parallel.take().expect("parallel runtime exists");
            self.update_parallel_completion(&mut runtime, trace);
            self.parallel = Some(runtime);
            return;
        }
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
        if let Some(event) = compiled.event_name(completion) {
            trace.record(StateTreeTraceRecord::InternalEventRaised { event });
        }
    }

    fn update_parallel_completion<T: StateTreeTrace + ?Sized>(
        &mut self,
        runtime: &mut ParallelRuntime,
        trace: &mut T,
    ) {
        let compiled = Arc::clone(
            &self
                .linked
                .as_ref()
                .expect("initialized instance has a definition")
                .compiled,
        );
        if compiled.children(compiled.root).iter().any(|child| {
            self.active.contains(child) && compiled.state(*child).kind == StateKind::Final
        }) {
            self.status = InstanceStatus::Complete;
            return;
        }

        for depth in (1..compiled.max_depth as u32).rev() {
            for state in
                self.active.iter().copied().filter(|state| {
                    compiled.state(*state).depth == depth && *state != compiled.root
                })
            {
                if runtime.completed.contains(&state) {
                    continue;
                }
                let complete = match compiled.state(state).kind {
                    StateKind::Compound { .. } => compiled.children(state).iter().any(|child| {
                        self.active.contains(child)
                            && compiled.state(*child).kind == StateKind::Final
                    }),
                    StateKind::Parallel { .. } => {
                        !compiled.children(state).is_empty()
                            && compiled
                                .children(state)
                                .iter()
                                .all(|child| runtime.completed.contains(child))
                    }
                    StateKind::Atomic | StateKind::Final => false,
                };
                if !complete {
                    continue;
                }
                runtime.completed.push(state);
                let completion = match compiled.state(state).kind {
                    StateKind::Compound { completion, .. } | StateKind::Parallel { completion } => {
                        completion
                    }
                    StateKind::Atomic | StateKind::Final => unreachable!(),
                };
                self.internal_events.push_back(completion);
                if let Some(event) = compiled.event_name(completion) {
                    trace.record(StateTreeTraceRecord::InternalEventRaised { event });
                }
            }
        }
    }

    fn finish_macrostep<T: StateTreeTrace + ?Sized>(
        &self,
        compiled: &CompiledDefinition,
        trace: &mut T,
    ) {
        trace.record(StateTreeTraceRecord::ConfigurationStarted);
        for state in &self.active {
            trace.record(StateTreeTraceRecord::ActiveState {
                state: &compiled.state(*state).name,
            });
        }
        trace.record(StateTreeTraceRecord::Status {
            status: &self.status,
        });
        trace.record(StateTreeTraceRecord::MacrostepEnded);
    }

    fn finish_current_macrostep<T: StateTreeTrace + ?Sized>(&self, trace: &mut T) {
        let Some(compiled) = self.linked.as_ref().map(|linked| linked.compiled.as_ref()) else {
            return;
        };
        self.finish_macrostep(compiled, trace);
    }
}

fn is_descendant(compiled: &CompiledDefinition, state: StateId, ancestor: StateId) -> bool {
    let mut cursor = compiled.state(state).parent;
    while let Some(current) = cursor {
        if current == ancestor {
            return true;
        }
        cursor = compiled.state(current).parent;
    }
    false
}

fn exits_domain(compiled: &CompiledDefinition, state: StateId, domain: Option<StateId>) -> bool {
    domain.is_none_or(|domain| is_descendant(compiled, state, domain))
}

fn transitions_conflict(
    compiled: &CompiledDefinition,
    left: SelectedTransition,
    right: SelectedTransition,
) -> bool {
    if left.transition.target.is_none() || right.transition.target.is_none() {
        return false;
    }
    match (left.transition.domain, right.transition.domain) {
        (None, _) | (_, None) => true,
        (Some(left), Some(right)) => {
            left == right
                || is_descendant(compiled, left, right)
                || is_descendant(compiled, right, left)
        }
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
    use crate::{StateTreeBuilder, StateTreeTransition};

    struct TestDomain;

    struct TestEvents;

    impl crate::StateTreeEvents for TestEvents {
        const AUTHORED_NAMES: &'static [&'static str] = &["advance", "finish"];

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
            Arc::new(
                LinkedDefinition::<TestDomain>::new(Arc::clone(&definition.compiled), None)
                    .unwrap(),
            ),
            limit,
            None,
            &mut RejectEffects,
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
            StateTreeExecution::<DescriptorEvents>::new(&definition, NonZeroUsize::new(8).unwrap())
                .unwrap();

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
    fn initializes_parallel_regions_in_hierarchy_order() {
        let mut builder = StateTreeBuilder::new("parallel");
        let root = builder.compound_root("root").unwrap();
        let parallel = builder.parallel("parallel", root).unwrap();
        let left = builder.compound("left", parallel).unwrap();
        let right = builder.compound("right", parallel).unwrap();
        // Builder insertion order need not already be depth-first.
        let left_leaf = builder.atomic("left-leaf", left).unwrap();
        let right_leaf = builder.atomic("right-leaf", right).unwrap();
        builder.initial(root, parallel).unwrap();
        builder.initial(left, left_leaf).unwrap();
        builder.initial(right, right_leaf).unwrap();
        let definition = builder.build().unwrap();

        let instance = instance(&definition, 64);

        assert_eq!(
            instance.active_state_names().collect::<Vec<_>>(),
            [
                "root",
                "parallel",
                "left",
                "left-leaf",
                "right",
                "right-leaf"
            ]
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

    #[test]
    fn targetless_completion_handler_does_not_repeat_completion() {
        let mut builder = StateTreeBuilder::new("targetless-completion");
        let root = builder.compound_root("root").unwrap();
        let nested = builder.compound("nested", root).unwrap();
        let running = builder.atomic("running", nested).unwrap();
        let done = builder.final_state("done", nested).unwrap();
        builder.initial(root, nested).unwrap();
        builder.initial(nested, running).unwrap();
        builder.transition(running, "finish", done).unwrap();
        builder
            .add_transition(nested, StateTreeTransition::on("done.state.nested"))
            .unwrap();
        let definition = builder.build().unwrap();
        let mut instance = instance(&definition, 3);

        instance.process_authored_event("finish", 3);

        assert_eq!(instance.status(), &InstanceStatus::Running);
        assert_eq!(
            instance.active_state_names().collect::<Vec<_>>(),
            ["root", "nested", "done"]
        );
    }
}
