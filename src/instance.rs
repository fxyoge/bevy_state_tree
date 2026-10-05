use std::{
    collections::VecDeque,
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use bevy_asset::{AssetId, Handle};
use bevy_ecs::{
    component::Component, entity::Entity, lifecycle::HookContext, world::DeferredWorld,
};

use crate::activity::{
    ErasedActivityToken, RuntimeActivitySink, RuntimeActivityStart, StateTreeActivityInbox,
};
use crate::definition::{
    Action, ActivityId, CompiledDefinition, Entry, EventId, EventSet, ExpressionId, HistoryIndex,
    LinkedDefinition, StateId, StateKind, StateTreeTarget, TimerId, Transition, TransitionTrigger,
};
use crate::effect::{RuntimeEffectSink, StateTreeEffectArgs, StateTreeEffectContext};
use crate::{
    LuauValue, NoStateTreeTrace, StateTreeActivityArgs, StateTreeDefinition, StateTreeDomain,
    StateTreeEventRef, StateTreeHistoryResolution, StateTreeTrace, StateTreeTraceRecord,
    StateTreeTraceTrigger, luau,
    plugin::{StateTreeEventInbox, StateTreeTimers},
};

static NEXT_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);

/// Immutable instance-local values applied after authored datamodel initializers.
#[derive(Clone, Debug, Default)]
pub struct StateTreeDataOverrides {
    values: Arc<[(Box<str>, LuauValue)]>,
}

impl StateTreeDataOverrides {
    /// Creates overrides while rejecting duplicate authored data names.
    pub fn try_new<I, N>(values: I) -> Result<Self, StateTreeDataOverrideError>
    where
        I: IntoIterator<Item = (N, LuauValue)>,
        N: Into<Box<str>>,
    {
        let mut collected = Vec::<(Box<str>, LuauValue)>::new();
        for (name, value) in values {
            let name = name.into();
            if collected.iter().any(|(candidate, _)| candidate == &name) {
                return Err(StateTreeDataOverrideError::DuplicateName { name });
            }
            collected.push((name, value));
        }
        Ok(Self {
            values: collected.into(),
        })
    }
}

/// Invalid instance-local initial datamodel configuration.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum StateTreeDataOverrideError {
    /// One authored data name was supplied more than once.
    #[error("initial datamodel override `{name}` is duplicated")]
    DuplicateName { name: Box<str> },
}

enum Trigger<'a, D: StateTreeDomain> {
    Eventless(
        Option<&'a str>,
        Option<StateTreeEventRef<'a, D>>,
        Option<&'a LuauValue>,
    ),
    Internal(EventId, Option<&'a LuauValue>),
    External(&'a EventSet, StateTreeEventRef<'a, D>),
    After(&'a [TimerToken]),
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct TimerToken {
    pub runtime_id: u64,
    pub timer: TimerId,
    pub generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TimerUpdate {
    Arm {
        token: TimerToken,
        deadline: Duration,
    },
    Cancel(TimerToken),
}

#[derive(Clone, Copy, Debug, Default)]
struct TimerSlot {
    generation: u64,
    armed: bool,
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

#[derive(Debug)]
struct ParallelRuntime {
    selected: Vec<SelectedTransition>,
    exited: Vec<StateId>,
    entries: Vec<Entry>,
    completed: Vec<StateId>,
    selected_sources: Box<[bool]>,
    selected_domains: Box<[bool]>,
    selected_domain_descendants: Box<[u32]>,
    selected_targetful: usize,
    selected_root_domain: bool,
    exited_states: Box<[bool]>,
    entry_indices: Box<[u32]>,
    completed_states: Box<[bool]>,
}

impl ParallelRuntime {
    fn new(state_count: usize, max_active: usize) -> Self {
        Self {
            selected: Vec::with_capacity(max_active),
            exited: Vec::with_capacity(max_active),
            entries: Vec::with_capacity(max_active),
            completed: Vec::with_capacity(max_active),
            selected_sources: vec![false; state_count].into_boxed_slice(),
            selected_domains: vec![false; state_count].into_boxed_slice(),
            selected_domain_descendants: vec![0; state_count].into_boxed_slice(),
            selected_targetful: 0,
            selected_root_domain: false,
            exited_states: vec![false; state_count].into_boxed_slice(),
            entry_indices: vec![u32::MAX; state_count].into_boxed_slice(),
            completed_states: vec![false; state_count].into_boxed_slice(),
        }
    }
}

#[derive(Debug)]
struct HistoryRuntime {
    values: Box<[Option<StateId>]>,
    capturing: Box<[bool]>,
    entries: Vec<Entry>,
    alternate: Vec<Entry>,
}

struct ActionContext<'a, D: StateTreeDomain> {
    event_name: Option<&'a str>,
    external_event: Option<StateTreeEventRef<'a, D>>,
    event_data: Option<&'a LuauValue>,
    entity: Entity,
}

impl<D: StateTreeDomain> Clone for ActionContext<'_, D> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<D: StateTreeDomain> Copy for ActionContext<'_, D> {}

impl<'a, D: StateTreeDomain> Trigger<'a, D> {
    fn event_name(&self, compiled: &'a CompiledDefinition) -> Option<&'a str> {
        match self {
            Self::Eventless(name, _, _) => *name,
            Self::Internal(event, _) => compiled.event_name(*event),
            Self::External(_, event) => Some(event.authored_name()),
            Self::After(_) => None,
        }
    }

    fn external_event(&self) -> Option<StateTreeEventRef<'a, D>> {
        match self {
            Self::Eventless(_, event, _) => *event,
            Self::External(_, event) => Some(*event),
            Self::Internal(_, _) => None,
            Self::After(_) => None,
        }
    }

    fn event_data(&self) -> Option<&'a LuauValue> {
        match self {
            Self::Eventless(_, _, data) | Self::Internal(_, data) => *data,
            Self::External(..) | Self::After(_) => None,
        }
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
    /// Execution stopped because initialization or a macrostep failed.
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
    /// A typed activity request could not be constructed or started.
    Activity {
        activity: Box<str>,
        argument: Option<Box<str>>,
        span_start: usize,
        message: Box<str>,
    },
    /// A terminal activity value could not be projected into Luau.
    ActivityDataProjection {
        activity: Box<str>,
        message: Box<str>,
    },
    /// An instance supplied an initial value for data absent from its definition.
    UnknownDataOverride { name: Box<str> },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DatamodelOperation {
    Initialize,
    Condition,
    Assign,
    EffectArgument,
    ActivityArgument,
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
#[component(on_discard = release_runtime_state::<D>)]
#[require(PendingStateTree<D>, StateTreeEventInbox<D>)]
pub struct StateTreeInstance<D: StateTreeDomain> {
    runtime_id: u64,
    definition: Handle<StateTreeDefinition>,
    linked: Option<Arc<LinkedDefinition<D>>>,
    active: Vec<StateId>,
    active_states: Option<Box<[bool]>>,
    parallel: Option<Box<ParallelRuntime>>,
    internal_events: VecDeque<EventId>,
    now: Duration,
    timers: Vec<TimerSlot>,
    activities: Option<Box<[ActivitySlot]>>,
    history: Option<Box<HistoryRuntime>>,
    timer_updates: Vec<TimerUpdate>,
    initial_data: StateTreeDataOverrides,
    data: Vec<LuauValue>,
    status: InstanceStatus,
    marker: PhantomData<fn() -> D>,
}

#[derive(Clone, Copy, Debug, Default)]
struct ActivitySlot {
    generation: u64,
    active: bool,
}

fn release_runtime_state<D: StateTreeDomain>(mut world: DeferredWorld, context: HookContext) {
    let cancellations = world
        .get::<StateTreeInstance<D>>(context.entity)
        .map(|instance| instance.active_activity_tokens(context.entity))
        .unwrap_or_default();
    let timer_tokens = world
        .get::<StateTreeInstance<D>>(context.entity)
        .map(StateTreeInstance::armed_timer_tokens)
        .unwrap_or_default();
    if let Some(mut instance) = world.get_mut::<StateTreeInstance<D>>(context.entity) {
        instance.invalidate_activity_tokens();
    }
    for (handler, token) in cancellations {
        handler.cancel_deferred(token, &mut world);
    }
    if let Some(inbox) = world.get::<StateTreeActivityInbox<D>>(context.entity) {
        inbox
            .outcomes
            .lock()
            .expect("state-tree activity inbox poisoned")
            .clear();
    }
    if let Some(ingress) = world.get_resource::<crate::activity::StateTreeActivityIngress<D>>() {
        ingress
            .dirty
            .lock()
            .expect("state-tree activity dirty set poisoned")
            .remove(&context.entity);
    }
    if let Some(mut timers) = world.get_resource_mut::<StateTreeTimers<D>>() {
        timers.cancel_tokens(timer_tokens);
    }
    if let Some(traces) = world.get_resource::<crate::StateTreeTraceLog<D>>() {
        traces.discard(context.entity);
    }
    let mut entity = world.entity_mut(context.entity);
    let Some(mut inbox) = entity.get_mut::<StateTreeEventInbox<D>>() else {
        return;
    };
    inbox.release_storage();
}

impl<D: StateTreeDomain> StateTreeInstance<D> {
    /// Creates an instance waiting for its definition asset.
    pub fn new(definition: Handle<StateTreeDefinition>) -> Self {
        Self::new_with_initial_data(definition, StateTreeDataOverrides::default())
    }

    /// Creates an instance with values applied after authored datamodel initializers.
    pub fn new_with_initial_data(
        definition: Handle<StateTreeDefinition>,
        initial_data: StateTreeDataOverrides,
    ) -> Self {
        Self {
            runtime_id: NEXT_INSTANCE_ID.fetch_add(1, Ordering::Relaxed),
            definition,
            linked: None,
            active: Vec::new(),
            active_states: None,
            parallel: None,
            internal_events: VecDeque::new(),
            now: Duration::ZERO,
            timers: Vec::new(),
            activities: None,
            history: None,
            timer_updates: Vec::new(),
            initial_data,
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

    pub(crate) fn set_time(&mut self, now: Duration) {
        self.now = now;
    }

    pub(crate) fn drain_timer_updates(&mut self) -> std::vec::Drain<'_, TimerUpdate> {
        self.timer_updates.drain(..)
    }

    fn armed_timer_tokens(&self) -> Vec<TimerToken> {
        self.timers
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.armed)
            .map(|(index, slot)| TimerToken {
                runtime_id: self.runtime_id,
                timer: TimerId(u32::try_from(index).expect("compiled timer index fits u32")),
                generation: slot.generation,
            })
            .collect()
    }

    fn active_activity_tokens(
        &self,
        entity: Entity,
    ) -> Vec<(
        Arc<dyn crate::activity::RegisteredActivity<D>>,
        ErasedActivityToken,
    )> {
        let Some(linked) = &self.linked else {
            return Vec::new();
        };
        let Some(activities) = self.activities.as_ref() else {
            return Vec::new();
        };
        activities
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.active)
            .map(|(index, slot)| {
                let id = ActivityId(index as u32);
                (
                    Arc::clone(linked.activity(id)),
                    ErasedActivityToken {
                        entity,
                        runtime_id: self.runtime_id,
                        activity: id,
                        generation: slot.generation,
                    },
                )
            })
            .collect()
    }

    fn invalidate_activity_tokens(&mut self) {
        self.runtime_id = NEXT_INSTANCE_ID.fetch_add(1, Ordering::Relaxed);
        let Some(activities) = self.activities.as_mut() else {
            return;
        };
        for slot in activities {
            slot.generation = slot.generation.wrapping_add(1);
            slot.active = false;
        }
    }

    pub(crate) fn accepts_activity_token(&self, token: ErasedActivityToken) -> bool {
        self.status == InstanceStatus::Running
            && token.runtime_id == self.runtime_id
            && self
                .activities
                .as_deref()
                .and_then(|activities| activities.get(token.activity.index()))
                .is_some_and(|slot| slot.active && slot.generation == token.generation)
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

    fn is_state_active(&self, state: StateId) -> bool {
        self.active_states.as_deref().map_or_else(
            || self.active.contains(&state),
            |states| states[state.index()],
        )
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

    pub(crate) fn initialize<
        S: RuntimeEffectSink<D> + ?Sized,
        A: RuntimeActivitySink<D> + ?Sized,
    >(
        &mut self,
        linked: Arc<LinkedDefinition<D>>,
        microstep_limit: usize,
        entity: Entity,
        effects: &mut S,
        activities: &mut A,
    ) {
        self.initialize_traced(
            linked,
            microstep_limit,
            entity,
            effects,
            activities,
            &mut NoStateTreeTrace,
        );
    }

    pub(crate) fn initialize_traced<
        S: RuntimeEffectSink<D> + ?Sized,
        A: RuntimeActivitySink<D> + ?Sized,
        T: StateTreeTrace + ?Sized,
    >(
        &mut self,
        linked: Arc<LinkedDefinition<D>>,
        microstep_limit: usize,
        entity: Entity,
        effects: &mut S,
        activities: &mut A,
        trace: &mut T,
    ) {
        let compiled = &linked.compiled;
        trace.record(StateTreeTraceRecord::MacrostepStarted {
            trigger: StateTreeTraceTrigger::Initialization,
        });
        self.linked = Some(Arc::clone(&linked));
        self.active.clear();
        self.active.reserve(compiled.max_active);
        self.active_states = compiled
            .has_parallel
            .then(|| vec![false; compiled.states.len()].into_boxed_slice());
        self.parallel = compiled.has_parallel.then(|| {
            Box::new(ParallelRuntime::new(
                compiled.states.len(),
                compiled.max_active,
            ))
        });
        self.internal_events.clear();
        self.internal_events
            .reserve(compiled.max_depth.max(compiled.max_active));
        self.timers.clear();
        self.timers
            .resize(compiled.timers.len(), TimerSlot::default());
        self.activities = (!compiled.activities.is_empty())
            .then(|| vec![ActivitySlot::default(); compiled.activities.len()].into_boxed_slice());
        self.history = (!compiled.history_slots.is_empty()).then(|| {
            Box::new(HistoryRuntime {
                values: vec![None; compiled.history_slots.len()].into_boxed_slice(),
                capturing: vec![false; compiled.history_slots.len()].into_boxed_slice(),
                entries: Vec::with_capacity(compiled.max_active),
                alternate: Vec::with_capacity(compiled.max_active),
            })
        });
        self.timer_updates.clear();
        self.data.clear();
        self.status = InstanceStatus::Running;
        if let Err(fault) = self.initialize_datamodel(compiled, trace) {
            self.set_fault(fault, entity, activities, trace);
            self.finish_macrostep(compiled, trace);
            return;
        }
        let initial_entries = if compiled
            .initial_entries
            .iter()
            .any(|entry| entry.history_default.is_some())
        {
            let mut history = self.history.take().expect("history entry requires runtime");
            materialize_history_entries(
                compiled,
                &history.values,
                &compiled.initial_entries,
                None,
                &mut history.entries,
                &mut history.alternate,
                trace,
            );
            let entries = std::mem::take(&mut history.entries);
            self.history = Some(history);
            Some(entries)
        } else {
            None
        };
        let entries = initial_entries
            .as_deref()
            .unwrap_or(&compiled.initial_entries);
        for entry in entries {
            if let Err(fault) = self.enter_state(
                compiled,
                *entry,
                ActionContext {
                    event_name: None,
                    external_event: None,
                    event_data: None,
                    entity,
                },
                effects,
                activities,
                trace,
            ) {
                self.set_fault(fault, entity, activities, trace);
                self.finish_macrostep(compiled, trace);
                return;
            }
        }
        if let Some(entries) = initial_entries {
            self.history
                .as_mut()
                .expect("history runtime restored")
                .entries = entries;
        }
        self.update_completion(entity, activities, trace);
        if self.status == InstanceStatus::Running {
            self.stabilize(
                microstep_limit,
                0,
                ActionContext {
                    event_name: None,
                    external_event: None,
                    event_data: None,
                    entity,
                },
                effects,
                activities,
                trace,
            );
        }
        self.finish_macrostep(compiled, trace);
    }

    pub(crate) fn process_event<
        S: RuntimeEffectSink<D> + ?Sized,
        A: RuntimeActivitySink<D> + ?Sized,
    >(
        &mut self,
        event: StateTreeEventRef<'_, D>,
        microstep_limit: usize,
        entity: Entity,
        effects: &mut S,
        activities: &mut A,
    ) -> bool {
        self.process_event_traced(
            event,
            microstep_limit,
            entity,
            effects,
            activities,
            &mut NoStateTreeTrace,
        )
    }

    pub(crate) fn process_event_traced<
        S: RuntimeEffectSink<D> + ?Sized,
        A: RuntimeActivitySink<D> + ?Sized,
        T: StateTreeTrace + ?Sized,
    >(
        &mut self,
        event: StateTreeEventRef<'_, D>,
        microstep_limit: usize,
        entity: Entity,
        effects: &mut S,
        activities: &mut A,
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
            let Some(events) = linked.external_event(event.registered_event_id().index()) else {
                return false;
            };
            self.select_step(Trigger::External(events, event))
        } else {
            let Some(linked) = self.linked.as_ref() else {
                return false;
            };
            let Some(events) = linked.external_event(event.registered_event_id().index()) else {
                return false;
            };
            self.select_transition(Trigger::External(events, event))
                .map(|selected| selected.map(SelectedStep::Single))
        };
        let transition = match selected {
            Ok(Some(transition)) => transition,
            Ok(None) => {
                self.finish_current_macrostep(trace);
                return false;
            }
            Err(fault) => {
                self.set_fault(fault, entity, activities, trace);
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
                event_data: None,
                entity,
            },
            effects,
            activities,
            trace,
        ) {
            self.set_fault(fault, entity, activities, trace);
            self.finish_current_macrostep(trace);
            return true;
        }
        if configuration_changed {
            self.update_completion(entity, activities, trace);
        }
        if self.status == InstanceStatus::Running {
            self.stabilize(
                microstep_limit,
                1,
                ActionContext {
                    event_name: Some(event_name),
                    external_event: Some(event),
                    event_data: None,
                    entity,
                },
                effects,
                activities,
                trace,
            );
        }
        self.finish_current_macrostep(trace);
        true
    }

    pub(crate) fn process_timers<
        S: RuntimeEffectSink<D> + ?Sized,
        A: RuntimeActivitySink<D> + ?Sized,
    >(
        &mut self,
        tokens: &[TimerToken],
        microstep_limit: usize,
        entity: Entity,
        effects: &mut S,
        activities: &mut A,
    ) -> bool {
        self.process_timers_traced(
            tokens,
            microstep_limit,
            entity,
            effects,
            activities,
            &mut NoStateTreeTrace,
        )
    }

    pub(crate) fn process_timers_traced<
        S: RuntimeEffectSink<D> + ?Sized,
        A: RuntimeActivitySink<D> + ?Sized,
        T: StateTreeTrace + ?Sized,
    >(
        &mut self,
        tokens: &[TimerToken],
        microstep_limit: usize,
        entity: Entity,
        effects: &mut S,
        activities: &mut A,
        trace: &mut T,
    ) -> bool {
        if self.status != InstanceStatus::Running {
            return false;
        }
        let mut any_due = false;
        for token in tokens {
            if token.runtime_id != self.runtime_id {
                continue;
            }
            let Some(slot) = self.timers.get_mut(token.timer.index()) else {
                continue;
            };
            if !slot.armed || slot.generation != token.generation {
                continue;
            }
            slot.armed = false;
            any_due = true;
        }
        if !any_due {
            return false;
        }
        trace.record(StateTreeTraceRecord::MacrostepStarted {
            trigger: StateTreeTraceTrigger::Timer,
        });
        let transition = match self.select_step(Trigger::After(tokens)) {
            Ok(Some(transition)) => transition,
            Ok(None) => {
                self.finish_current_macrostep(trace);
                return false;
            }
            Err(fault) => {
                self.set_fault(fault, entity, activities, trace);
                self.finish_current_macrostep(trace);
                return true;
            }
        };
        let configuration_changed = self.selected_step_changes_configuration(transition);
        self.trace_selected_step(1, transition, trace);
        if let Err(fault) = self.take_selected_step(
            transition,
            ActionContext {
                event_name: None,
                external_event: None,
                event_data: None,
                entity,
            },
            effects,
            activities,
            trace,
        ) {
            self.set_fault(fault, entity, activities, trace);
            self.finish_current_macrostep(trace);
            return true;
        }
        if configuration_changed {
            self.update_completion(entity, activities, trace);
        }
        if self.status == InstanceStatus::Running {
            self.stabilize(
                microstep_limit,
                1,
                ActionContext {
                    event_name: None,
                    external_event: None,
                    event_data: None,
                    entity,
                },
                effects,
                activities,
                trace,
            );
        }
        self.finish_current_macrostep(trace);
        true
    }

    pub(crate) fn process_activity_outcome<
        S: RuntimeEffectSink<D> + ?Sized,
        A: RuntimeActivitySink<D> + ?Sized,
    >(
        &mut self,
        outcome: crate::activity::ActivityOutcome,
        microstep_limit: usize,
        entity: Entity,
        effects: &mut S,
        activities: &mut A,
    ) -> bool {
        self.process_activity_outcome_traced(
            outcome,
            microstep_limit,
            entity,
            effects,
            activities,
            &mut NoStateTreeTrace,
        )
    }

    pub(crate) fn process_activity_outcome_traced<
        S: RuntimeEffectSink<D> + ?Sized,
        A: RuntimeActivitySink<D> + ?Sized,
        T: StateTreeTrace + ?Sized,
    >(
        &mut self,
        outcome: crate::activity::ActivityOutcome,
        microstep_limit: usize,
        entity: Entity,
        effects: &mut S,
        activities: &mut A,
        trace: &mut T,
    ) -> bool {
        if self.status != InstanceStatus::Running || outcome.token.runtime_id != self.runtime_id {
            return false;
        }
        let Some(slot) = self
            .activities
            .as_mut()
            .and_then(|activities| activities.get_mut(outcome.token.activity.index()))
        else {
            return false;
        };
        if !slot.active || slot.generation != outcome.token.generation {
            return false;
        }
        slot.active = false;
        let compiled = Arc::clone(
            &self
                .linked
                .as_ref()
                .expect("running instance is linked")
                .compiled,
        );
        let definition = &compiled.activities[outcome.token.activity.index()];
        if !self.is_state_active(definition.owner) {
            return false;
        }
        let success = matches!(outcome.kind, crate::activity::ActivityOutcomeKind::Complete);
        let event = if success {
            definition.done
        } else {
            definition.error
        };
        trace.record(StateTreeTraceRecord::MacrostepStarted {
            trigger: StateTreeTraceTrigger::ActivityOutcome(&definition.id),
        });
        trace.record(StateTreeTraceRecord::ActivityOutcome {
            activity: &definition.id,
            success,
            generation: outcome.token.generation,
        });
        let data = match outcome.data {
            Ok(data) => data,
            Err(error) => {
                self.set_fault(
                    RuntimeFault::ActivityDataProjection {
                        activity: definition.id.clone(),
                        message: error.to_string().into(),
                    },
                    entity,
                    activities,
                    trace,
                );
                self.finish_current_macrostep(trace);
                return true;
            }
        };
        let context = ActionContext {
            event_name: compiled.event_name(event),
            external_event: None,
            event_data: data.as_ref(),
            entity,
        };
        let selected = self.select_step(Trigger::Internal(event, data.as_ref()));
        match selected {
            Ok(Some(selected)) => {
                let changed = self.selected_step_changes_configuration(selected);
                self.trace_selected_step(1, selected, trace);
                if let Err(fault) =
                    self.take_selected_step(selected, context, effects, activities, trace)
                {
                    self.set_fault(fault, entity, activities, trace);
                } else {
                    if changed {
                        self.update_completion(entity, activities, trace);
                    }
                    if self.status == InstanceStatus::Running {
                        self.stabilize(microstep_limit, 1, context, effects, activities, trace);
                    }
                }
            }
            Ok(None) => {
                self.stabilize(microstep_limit, 0, context, effects, activities, trace);
            }
            Err(fault) => self.set_fault(fault, entity, activities, trace),
        }
        self.finish_current_macrostep(trace);
        true
    }

    fn stabilize<
        S: RuntimeEffectSink<D> + ?Sized,
        A: RuntimeActivitySink<D> + ?Sized,
        T: StateTreeTrace + ?Sized,
    >(
        &mut self,
        limit: usize,
        mut transitions_taken: usize,
        context: ActionContext<'_, D>,
        effects: &mut S,
        activities: &mut A,
        trace: &mut T,
    ) {
        let compiled = self
            .linked
            .as_ref()
            .map(|linked| Arc::clone(&linked.compiled));
        let mut current_event = context.event_name;
        let mut external_event = context.external_event;
        let mut event_data = context.event_data;
        loop {
            let transition = match self.select_step(Trigger::Eventless(
                current_event,
                external_event,
                event_data,
            )) {
                Ok(Some(transition)) => transition,
                Ok(None) => {
                    let Some(event) = self.internal_events.pop_front() else {
                        return;
                    };
                    current_event = compiled
                        .as_ref()
                        .and_then(|compiled| compiled.event_name(event));
                    external_event = None;
                    event_data = None;
                    if let Some(event_name) = current_event {
                        trace.record(StateTreeTraceRecord::InternalEventConsumed {
                            event: event_name,
                        });
                    }
                    match self.select_step(Trigger::Internal(event, None)) {
                        Ok(Some(transition)) => transition,
                        Ok(None) => continue,
                        Err(fault) => {
                            self.set_fault(fault, context.entity, activities, trace);
                            return;
                        }
                    }
                }
                Err(fault) => {
                    self.set_fault(fault, context.entity, activities, trace);
                    return;
                }
            };
            if transitions_taken == limit {
                self.set_fault(
                    RuntimeFault::MicrostepLimitExceeded { limit },
                    context.entity,
                    activities,
                    trace,
                );
                return;
            }
            let configuration_changed = self.selected_step_changes_configuration(transition);
            self.trace_selected_step(transitions_taken + 1, transition, trace);
            if let Err(fault) = self.take_selected_step(
                transition,
                ActionContext {
                    event_name: current_event,
                    external_event,
                    event_data,
                    entity: context.entity,
                },
                effects,
                activities,
                trace,
            ) {
                self.set_fault(fault, context.entity, activities, trace);
                return;
            }
            transitions_taken += 1;
            if configuration_changed {
                self.update_completion(context.entity, activities, trace);
            }
            if self.status != InstanceStatus::Running {
                return;
            }
        }
    }

    fn select_transition(
        &self,
        trigger: Trigger<'_, D>,
    ) -> Result<Option<SelectedTransition>, RuntimeFault> {
        let Some(compiled) = self.linked.as_ref().map(|linked| &linked.compiled) else {
            return Ok(None);
        };
        for state in self.active.iter().rev() {
            for (index, transition) in compiled.state(*state).transitions.iter().enumerate() {
                let matches = match (transition.trigger, &trigger) {
                    (TransitionTrigger::Eventless, Trigger::Eventless(_, _, _)) => true,
                    (TransitionTrigger::Event(candidate), Trigger::Internal(emitted, _)) => {
                        compiled.internal_event_matches(candidate, *emitted)
                    }
                    (TransitionTrigger::Event(event), Trigger::External(events, _)) => {
                        events.contains(event)
                    }
                    (TransitionTrigger::After(timer), Trigger::After(due)) => {
                        self.timer_matches(due, timer)
                    }
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
                        trigger.event_data(),
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
        trigger: Trigger<'_, D>,
    ) -> Result<Option<SelectedStep>, RuntimeFault> {
        if self.parallel.is_none() {
            return self
                .select_transition(trigger)
                .map(|selected| selected.map(SelectedStep::Single));
        }

        let mut runtime = self.parallel.take().expect("parallel runtime exists");
        let result = self.select_parallel_transition_set(&trigger, &mut runtime);
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
        trigger: &Trigger<'_, D>,
        runtime: &mut ParallelRuntime,
    ) -> Result<(), RuntimeFault> {
        runtime.selected.clear();
        runtime.selected_sources.fill(false);
        runtime.selected_domains.fill(false);
        runtime.selected_domain_descendants.fill(0);
        runtime.selected_targetful = 0;
        runtime.selected_root_domain = false;
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
                    let matches = match (transition.trigger, trigger) {
                        (TransitionTrigger::Eventless, Trigger::Eventless(_, _, _)) => true,
                        (TransitionTrigger::Event(candidate), Trigger::Internal(emitted, _)) => {
                            compiled.internal_event_matches(candidate, *emitted)
                        }
                        (TransitionTrigger::Event(event), Trigger::External(events, _)) => {
                            events.contains(event)
                        }
                        (TransitionTrigger::After(timer), Trigger::After(due)) => {
                            self.timer_matches(due, timer)
                        }
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
                            trigger.event_data(),
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
            if runtime.selected_sources[candidate.source.index()]
                && runtime.selected.iter().any(|selected| {
                    selected.source == candidate.source && selected.index == candidate.index
                })
            {
                continue;
            }

            let mut accepted = true;
            if parallel_candidate_may_conflict(compiled, runtime, candidate) {
                let mut index = 0;
                while index < runtime.selected.len() {
                    if !transitions_conflict(compiled, runtime.selected[index], candidate) {
                        index += 1;
                        continue;
                    }
                    if is_descendant(compiled, candidate.source, runtime.selected[index].source) {
                        let removed = runtime.selected.remove(index);
                        unmark_selected_transition(compiled, runtime, removed);
                    } else {
                        accepted = false;
                        break;
                    }
                }
            }
            if accepted {
                mark_selected_transition(compiled, runtime, candidate);
                runtime.selected.push(candidate);
            }
        }
        runtime
            .selected
            .sort_unstable_by_key(|candidate| compiled.state(candidate.source).document_order);
        Ok(())
    }

    fn take_selected_step<
        S: RuntimeEffectSink<D> + ?Sized,
        A: RuntimeActivitySink<D> + ?Sized,
        T: StateTreeTrace + ?Sized,
    >(
        &mut self,
        selected: SelectedStep,
        context: ActionContext<'_, D>,
        effects: &mut S,
        activities: &mut A,
        trace: &mut T,
    ) -> Result<(), RuntimeFault> {
        match selected {
            SelectedStep::Single(selected) => {
                self.take_transition(selected, context, effects, activities, trace)
            }
            SelectedStep::Parallel => {
                let mut runtime = self.parallel.take().expect("parallel runtime exists");
                let result = self.take_parallel_transition_set(
                    &mut runtime,
                    context,
                    effects,
                    activities,
                    trace,
                );
                self.parallel = Some(runtime);
                result
            }
        }
    }

    fn take_parallel_transition_set<
        S: RuntimeEffectSink<D> + ?Sized,
        A: RuntimeActivitySink<D> + ?Sized,
        T: StateTreeTrace + ?Sized,
    >(
        &mut self,
        runtime: &mut ParallelRuntime,
        context: ActionContext<'_, D>,
        effects: &mut S,
        activities: &mut A,
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
        runtime.exited_states.fill(false);
        if runtime.selected_targetful != 0 {
            for state in &self.active {
                let exits = if runtime.selected_root_domain {
                    true
                } else {
                    let mut cursor = compiled.state(*state).parent;
                    let mut exits = false;
                    while let Some(ancestor) = cursor {
                        if runtime.selected_domains[ancestor.index()] {
                            exits = true;
                            break;
                        }
                        cursor = compiled.state(ancestor).parent;
                    }
                    exits
                };
                if exits {
                    runtime.exited_states[state.index()] = true;
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

        capture_history_values(
            &compiled,
            &self.active,
            &runtime.exited,
            self.history.as_deref_mut(),
        );

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
            self.cancel_state_timers(&compiled, *state);
            self.cancel_state_activities(*state, context.entity, activities, trace);
        }
        for state in &runtime.exited {
            self.active_states
                .as_mut()
                .expect("parallel definitions track active states")[state.index()] = false;
            runtime.completed_states[state.index()] = false;
        }
        self.active
            .retain(|state| !runtime.exited_states[state.index()]);
        runtime
            .completed
            .retain(|state| !runtime.exited_states[state.index()]);

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
        runtime.entry_indices.fill(u32::MAX);
        let mut history_runtime = self.history.take();
        for selected in &runtime.selected {
            let start = selected.transition.entry_start as usize;
            let end = start + selected.transition.entry_len as usize;
            let source_entries = &compiled.transition_entries[start..end];
            let entries = if let Some(history) = history_runtime.as_mut()
                && (matches!(
                    selected.transition.target,
                    Some(StateTreeTarget::History(_))
                ) || source_entries
                    .iter()
                    .any(|entry| entry.history_default.is_some()))
            {
                materialize_history_entries(
                    &compiled,
                    &history.values,
                    source_entries,
                    selected.transition.target,
                    &mut history.entries,
                    &mut history.alternate,
                    trace,
                );
                history.entries.as_slice()
            } else {
                source_entries
            };
            for entry in entries {
                if self.is_state_active(entry.state) {
                    continue;
                }
                let index = runtime.entry_indices[entry.state.index()];
                if index != u32::MAX {
                    runtime.entries[index as usize].take_initial |= entry.take_initial;
                } else {
                    runtime.entry_indices[entry.state.index()] =
                        u32::try_from(runtime.entries.len())
                            .expect("compiled state count fits u32");
                    runtime.entries.push(*entry);
                }
            }
        }
        self.history = history_runtime;
        runtime
            .entries
            .sort_unstable_by_key(|entry| compiled.state(entry.state).document_order);
        for index in 0..runtime.entries.len() {
            let entry = runtime.entries[index];
            if let Err(fault) =
                self.enter_state(&compiled, entry, context, effects, activities, trace)
            {
                self.active
                    .sort_unstable_by_key(|state| compiled.state(*state).document_order);
                return Err(fault);
            }
        }
        self.active
            .sort_unstable_by_key(|state| compiled.state(*state).document_order);
        Ok(())
    }

    fn take_transition<
        S: RuntimeEffectSink<D> + ?Sized,
        A: RuntimeActivitySink<D> + ?Sized,
        T: StateTreeTrace + ?Sized,
    >(
        &mut self,
        selected: SelectedTransition,
        context: ActionContext<'_, D>,
        effects: &mut S,
        activities: &mut A,
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
        capture_history_values(
            &compiled,
            &self.active,
            &self.active[exit_depth..],
            self.history.as_deref_mut(),
        );
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
            self.cancel_state_timers(&compiled, state);
            self.cancel_state_activities(state, context.entity, activities, trace);
        }
        if let Some(active_states) = self.active_states.as_mut() {
            for state in &self.active[exit_depth..] {
                active_states[state.index()] = false;
            }
        }
        if let Some(runtime) = self.parallel.as_mut() {
            for state in &self.active[exit_depth..] {
                runtime.completed_states[state.index()] = false;
            }
            runtime
                .completed
                .retain(|state| runtime.completed_states[state.index()]);
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
        let source_entries = &compiled.transition_entries[start..end];
        if self.history.is_some()
            && (matches!(transition.target, Some(StateTreeTarget::History(_)))
                || source_entries
                    .iter()
                    .any(|entry| entry.history_default.is_some()))
        {
            let mut history = self.history.take().expect("checked history runtime");
            materialize_history_entries(
                &compiled,
                &history.values,
                source_entries,
                transition.target,
                &mut history.entries,
                &mut history.alternate,
                trace,
            );
            let entries = std::mem::take(&mut history.entries);
            self.history = Some(history);
            for entry in &entries {
                self.enter_state(&compiled, *entry, context, effects, activities, trace)?;
            }
            self.history
                .as_mut()
                .expect("history runtime restored")
                .entries = entries;
        } else {
            for entry in source_entries {
                self.enter_state(&compiled, *entry, context, effects, activities, trace)?;
            }
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
            trace_transition_target(compiled, target, trace);
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
                        trace_transition_target(compiled, target, trace);
                    } else {
                        trace.record(StateTreeTraceRecord::TransitionTargetless);
                    }
                }
            }
        }
    }

    fn enter_state<
        S: RuntimeEffectSink<D> + ?Sized,
        A: RuntimeActivitySink<D> + ?Sized,
        T: StateTreeTrace + ?Sized,
    >(
        &mut self,
        compiled: &CompiledDefinition,
        entry: Entry,
        context: ActionContext<'_, D>,
        effects: &mut S,
        activities: &mut A,
        trace: &mut T,
    ) -> Result<(), RuntimeFault> {
        self.active.push(entry.state);
        if let Some(active_states) = self.active_states.as_mut() {
            active_states[entry.state.index()] = true;
        }
        let state = compiled.state(entry.state);
        trace.record(StateTreeTraceRecord::StateEntered { state: &state.name });
        self.arm_state_timers(compiled, entry.state);
        self.execute_actions(compiled, &state.entry_actions, context, effects, trace)?;
        self.start_state_activities(compiled, entry.state, context, activities, trace)?;
        if entry.take_initial {
            self.execute_actions(compiled, &state.initial_actions, context, effects, trace)?;
        }
        if let Some(history_id) = entry.history_default {
            let history = compiled.history_by_index(history_id);
            let start = history.action_start as usize;
            let end = start + history.action_len as usize;
            self.execute_actions(
                compiled,
                &compiled.transition_actions[start..end],
                context,
                effects,
                trace,
            )?;
        }
        Ok(())
    }

    fn arm_state_timers(&mut self, compiled: &CompiledDefinition, state: StateId) {
        for transition in &compiled.state(state).transitions {
            let TransitionTrigger::After(timer) = transition.trigger else {
                continue;
            };
            let slot = &mut self.timers[timer.index()];
            slot.generation = slot.generation.wrapping_add(1);
            slot.armed = true;
            let Some(deadline) = self.now.checked_add(compiled.timers[timer.index()]) else {
                continue;
            };
            self.timer_updates.push(TimerUpdate::Arm {
                token: TimerToken {
                    runtime_id: self.runtime_id,
                    timer,
                    generation: slot.generation,
                },
                deadline,
            });
        }
    }

    fn timer_matches(&self, due: &[TimerToken], timer: TimerId) -> bool {
        let Ok(index) = due.binary_search_by_key(&timer, |token| token.timer) else {
            return false;
        };
        let token = due[index];
        token.runtime_id == self.runtime_id
            && self.timers[timer.index()].generation == token.generation
    }

    fn cancel_state_timers(&mut self, compiled: &CompiledDefinition, state: StateId) {
        for transition in &compiled.state(state).transitions {
            let TransitionTrigger::After(timer) = transition.trigger else {
                continue;
            };
            let slot = &mut self.timers[timer.index()];
            if !slot.armed {
                continue;
            }
            slot.armed = false;
            self.timer_updates.push(TimerUpdate::Cancel(TimerToken {
                runtime_id: self.runtime_id,
                timer,
                generation: slot.generation,
            }));
        }
    }

    fn cancel_all_timers(&mut self) {
        for (index, slot) in self.timers.iter_mut().enumerate() {
            if !slot.armed {
                continue;
            }
            slot.armed = false;
            self.timer_updates.push(TimerUpdate::Cancel(TimerToken {
                runtime_id: self.runtime_id,
                timer: TimerId(u32::try_from(index).expect("compiled timer index fits u32")),
                generation: slot.generation,
            }));
        }
    }

    fn start_state_activities<A: RuntimeActivitySink<D> + ?Sized, T: StateTreeTrace + ?Sized>(
        &mut self,
        compiled: &CompiledDefinition,
        state: StateId,
        context: ActionContext<'_, D>,
        sink: &mut A,
        trace: &mut T,
    ) -> Result<(), RuntimeFault> {
        let node = compiled.state(state);
        let start = node.activity_start as usize;
        let end = start + node.activity_len as usize;
        for index in start..end {
            let id = ActivityId(index as u32);
            let definition = &compiled.activities[index];
            let argument_start = definition.argument_start as usize;
            let argument_end = argument_start + definition.argument_len as usize;
            let mut values = Vec::with_capacity(definition.argument_len as usize);
            for argument in &compiled.named_arguments[argument_start..argument_end] {
                values.push((
                    argument.name.clone(),
                    self.evaluate_expression(
                        compiled,
                        argument.expression,
                        context.event_name,
                        context.external_event,
                        context.event_data,
                        DatamodelOperation::ActivityArgument,
                    )?,
                ));
            }
            let slot = &mut self
                .activities
                .as_mut()
                .expect("compiled activities require activity slot storage")[index];
            slot.generation = slot.generation.wrapping_add(1);
            slot.active = false;
            let generation = slot.generation;
            let token = ErasedActivityToken {
                entity: context.entity,
                runtime_id: self.runtime_id,
                activity: id,
                generation,
            };
            let handler = self
                .linked
                .as_ref()
                .expect("running instance is linked")
                .activity(id);
            let (data_names, data) = compiled
                .datamodel
                .as_ref()
                .map_or((&[][..], &[][..]), |datamodel| {
                    (datamodel.names.as_ref(), self.data.as_slice())
                });
            let activity_context = StateTreeEffectContext {
                entity: context.entity,
                event_name: context.event_name,
                external_event: context.external_event,
                data_names,
                data,
            };
            if let Err(error) = sink.start(RuntimeActivityStart {
                handler,
                context: activity_context,
                arguments: StateTreeActivityArgs::new(values),
                token,
            }) {
                return Err(RuntimeFault::Activity {
                    activity: definition.name.clone(),
                    argument: error.argument_name().map(Into::into),
                    span_start: definition.span_start as usize,
                    message: error.message().into(),
                });
            }
            self.activities
                .as_mut()
                .expect("compiled activities require activity slot storage")[index]
                .active = true;
            trace.record(StateTreeTraceRecord::ActivityStarted {
                activity: &definition.id,
                generation,
            });
        }
        Ok(())
    }

    fn cancel_state_activities<A: RuntimeActivitySink<D> + ?Sized, T: StateTreeTrace + ?Sized>(
        &mut self,
        state: StateId,
        entity: Entity,
        sink: &mut A,
        trace: &mut T,
    ) {
        let Some(linked) = self.linked.as_ref() else {
            return;
        };
        let node = linked.compiled.state(state);
        if node.activity_len == 0 {
            return;
        }
        let start = node.activity_start as usize;
        let end = start + node.activity_len as usize;
        for index in start..end {
            let slot = &mut self
                .activities
                .as_mut()
                .expect("compiled activities require activity slot storage")[index];
            if !slot.active {
                continue;
            }
            slot.active = false;
            let id = ActivityId(index as u32);
            let token = ErasedActivityToken {
                entity,
                runtime_id: self.runtime_id,
                activity: id,
                generation: slot.generation,
            };
            sink.cancel(linked.activity(id), token);
            trace.record(StateTreeTraceRecord::ActivityCancelled {
                activity: &linked.compiled.activities[index].id,
                generation: slot.generation,
            });
        }
    }

    fn cancel_all_activities<A: RuntimeActivitySink<D> + ?Sized, T: StateTreeTrace + ?Sized>(
        &mut self,
        entity: Entity,
        sink: &mut A,
        trace: &mut T,
    ) {
        let states: Vec<_> = self.active.clone();
        for state in states {
            self.cancel_state_activities(state, entity, sink, trace);
        }
    }

    fn set_fault<A: RuntimeActivitySink<D> + ?Sized, T: StateTreeTrace + ?Sized>(
        &mut self,
        fault: RuntimeFault,
        entity: Entity,
        activities: &mut A,
        trace: &mut T,
    ) {
        self.status = InstanceStatus::Faulted(fault);
        self.cancel_all_timers();
        self.cancel_all_activities(entity, activities, trace);
    }

    fn initialize_datamodel<T: StateTreeTrace + ?Sized>(
        &mut self,
        compiled: &CompiledDefinition,
        trace: &mut T,
    ) -> Result<(), RuntimeFault> {
        let Some(datamodel) = &compiled.datamodel else {
            if let Some((name, _)) = self.initial_data.values.first() {
                return Err(RuntimeFault::UnknownDataOverride { name: name.clone() });
            }
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
                None,
                DatamodelOperation::Initialize,
            )?;
            trace.record(StateTreeTraceRecord::DataAssigned {
                data: &datamodel.names[index],
                value: &self.data[index],
            });
        }
        for (name, _) in self.initial_data.values.iter() {
            if !datamodel.names.iter().any(|candidate| candidate == name) {
                return Err(RuntimeFault::UnknownDataOverride { name: name.clone() });
            }
        }
        for (name, value) in self.initial_data.values.iter() {
            let index = datamodel
                .names
                .iter()
                .position(|candidate| candidate == name)
                .expect("initial data names were validated");
            self.data[index] = value.clone();
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
        external_event: Option<StateTreeEventRef<'_, D>>,
        event_data: Option<&LuauValue>,
    ) -> Result<bool, RuntimeFault> {
        match self.evaluate_expression(
            compiled,
            expression,
            event_name,
            external_event,
            event_data,
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
        external_event: Option<StateTreeEventRef<'_, D>>,
        explicit_event_data: Option<&LuauValue>,
        operation: DatamodelOperation,
    ) -> Result<LuauValue, RuntimeFault> {
        let datamodel = compiled
            .datamodel
            .as_ref()
            .expect("compiled Luau expressions require a datamodel");
        let expression = &datamodel.expressions[expression.index()];
        let projected_event_data = if expression.reads_event_data && explicit_event_data.is_none() {
            external_event
                .map(StateTreeEventRef::event_data)
                .transpose()
                .map_err(|error| RuntimeFault::EventDataProjection {
                    event: event_name.unwrap_or_default().into(),
                    message: error.to_string().into(),
                })?
                .flatten()
        } else {
            None
        };
        let event_data = explicit_event_data.or(projected_event_data.as_ref());
        luau::evaluate(
            expression,
            &datamodel.names,
            &self.data,
            event_name,
            event_data,
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
        context: ActionContext<'_, D>,
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
                        context.event_data,
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
                    for argument in &compiled.named_arguments[start..end] {
                        let value = self.evaluate_expression(
                            compiled,
                            argument.expression,
                            context.event_name,
                            context.external_event,
                            context.event_data,
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
                        .map(|linked| linked.effect(effect))
                        .expect("running instance is linked");
                    let handler = Arc::clone(handler);
                    let name = compiled.effect_name(effect);
                    let context = StateTreeEffectContext {
                        entity: context.entity,
                        event_name: context.event_name,
                        external_event: context.external_event,
                        data_names,
                        data,
                    };
                    if let Err(error) =
                        effects.emit(&handler, context, StateTreeEffectArgs::new(values))
                    {
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

    fn update_completion<A: RuntimeActivitySink<D> + ?Sized, T: StateTreeTrace + ?Sized>(
        &mut self,
        entity: Entity,
        activities: &mut A,
        trace: &mut T,
    ) {
        if self.parallel.is_some() {
            let mut runtime = self.parallel.take().expect("parallel runtime exists");
            self.update_parallel_completion(&mut runtime, entity, activities, trace);
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
            self.cancel_all_timers();
            self.cancel_all_activities(entity, activities, trace);
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

    fn update_parallel_completion<
        A: RuntimeActivitySink<D> + ?Sized,
        T: StateTreeTrace + ?Sized,
    >(
        &mut self,
        runtime: &mut ParallelRuntime,
        entity: Entity,
        activities: &mut A,
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
            self.is_state_active(*child) && compiled.state(*child).kind == StateKind::Final
        }) {
            self.status = InstanceStatus::Complete;
            self.cancel_all_timers();
            self.cancel_all_activities(entity, activities, trace);
            return;
        }

        for depth in (1..compiled.max_depth as u32).rev() {
            for state in
                self.active.iter().copied().filter(|state| {
                    compiled.state(*state).depth == depth && *state != compiled.root
                })
            {
                if runtime.completed_states[state.index()] {
                    continue;
                }
                let complete = match compiled.state(state).kind {
                    StateKind::Compound { .. } => compiled.children(state).iter().any(|child| {
                        self.is_state_active(*child)
                            && compiled.state(*child).kind == StateKind::Final
                    }),
                    StateKind::Parallel { .. } => {
                        !compiled.children(state).is_empty()
                            && compiled
                                .children(state)
                                .iter()
                                .all(|child| runtime.completed_states[child.index()])
                    }
                    StateKind::Atomic | StateKind::Final => false,
                };
                if !complete {
                    continue;
                }
                runtime.completed_states[state.index()] = true;
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

fn trace_transition_target<T: StateTreeTrace + ?Sized>(
    compiled: &CompiledDefinition,
    target: StateTreeTarget,
    trace: &mut T,
) {
    match target {
        StateTreeTarget::State(state) => trace.record(StateTreeTraceRecord::TransitionTarget {
            state: &compiled.state(state).name,
        }),
        StateTreeTarget::History(history_id) => {
            let history = compiled.history(history_id);
            trace.record(StateTreeTraceRecord::TransitionHistoryTarget {
                history: &history.name,
                owner: &compiled.state(history.owner).name,
            });
        }
    }
}

fn capture_history_values(
    compiled: &CompiledDefinition,
    active: &[StateId],
    exited: &[StateId],
    runtime: Option<&mut HistoryRuntime>,
) {
    let Some(runtime) = runtime else {
        return;
    };
    for owner in exited {
        let Some(slot) = compiled.state(*owner).history_slot else {
            continue;
        };
        match compiled.state(*owner).kind {
            StateKind::Compound { .. } => {
                runtime.values[slot.index()] = None;
                runtime.capturing[slot.index()] = true;
            }
            StateKind::Parallel { .. } => runtime.values[slot.index()] = Some(*owner),
            StateKind::Atomic | StateKind::Final => unreachable!(),
        }
    }
    for state in active {
        let Some(parent) = compiled.state(*state).parent else {
            continue;
        };
        let Some(slot) = compiled.state(parent).history_slot else {
            continue;
        };
        if runtime.capturing[slot.index()] {
            runtime.values[slot.index()] = Some(*state);
            runtime.capturing[slot.index()] = false;
        }
    }
    debug_assert!(runtime.capturing.iter().all(|capturing| !capturing));
}

fn materialize_history_entries<T: StateTreeTrace + ?Sized>(
    compiled: &CompiledDefinition,
    values: &[Option<StateId>],
    source: &[Entry],
    explicit_target: Option<StateTreeTarget>,
    entries: &mut Vec<Entry>,
    alternate: &mut Vec<Entry>,
    trace: &mut T,
) {
    entries.clear();
    entries.extend_from_slice(source);

    if let Some(StateTreeTarget::History(history_id)) = explicit_target
        && !entries
            .iter()
            .any(|entry| entry.history_default == Some(HistoryIndex::from_id(history_id)))
    {
        replace_history_descendants(
            compiled,
            values,
            HistoryIndex::from_id(history_id),
            entries,
            alternate,
            None,
            trace,
        );
    }

    let mut index = 0;
    while index < entries.len() {
        let Some(history_id) = entries[index].history_default else {
            index += 1;
            continue;
        };
        let stored = values[compiled.history_by_index(history_id).slot.index()].is_some();
        replace_history_descendants(
            compiled,
            values,
            history_id,
            entries,
            alternate,
            Some(index),
            trace,
        );
        if stored {
            entries[index].history_default = None;
        }
        index += 1;
    }
}

fn replace_history_descendants<T: StateTreeTrace + ?Sized>(
    compiled: &CompiledDefinition,
    values: &[Option<StateId>],
    history_id: HistoryIndex,
    entries: &mut Vec<Entry>,
    alternate: &mut Vec<Entry>,
    owner_index: Option<usize>,
    trace: &mut T,
) {
    let history = compiled.history_by_index(history_id);
    debug_assert_eq!(compiled.state(history.default).parent, Some(history.owner));
    let stored = values[history.slot.index()];
    trace.record(StateTreeTraceRecord::HistoryResolved {
        history: &history.name,
        resolution: if stored.is_some() {
            StateTreeHistoryResolution::Stored
        } else {
            StateTreeHistoryResolution::Default
        },
    });

    let replacement = if let Some(remembered) = stored {
        if matches!(
            compiled.state(history.owner).kind,
            StateKind::Parallel { .. }
        ) {
            compiled.history_entries(history.entry_start, history.entry_len)
        } else {
            let plans = compiled.history_child_plans(history.slot);
            let plan = plans
                .binary_search_by_key(&remembered.index(), |plan| plan.child.index())
                .map(|index| &plans[index])
                .expect("captured history child has a compiled entry plan");
            compiled.history_entries(plan.entry_start, plan.entry_len)
        }
    } else {
        compiled.history_entries(history.entry_start, history.entry_len)
    };

    let search_start = owner_index.map_or(0, |index| index + 1);
    let first = entries[search_start..]
        .iter()
        .position(|entry| is_descendant(compiled, entry.state, history.owner))
        .map_or(search_start, |index| search_start + index);
    let mut end = first;
    while end < entries.len() && is_descendant(compiled, entries[end].state, history.owner) {
        end += 1;
    }

    alternate.clear();
    alternate.extend_from_slice(&entries[..first]);
    alternate.extend_from_slice(replacement);
    alternate.extend_from_slice(&entries[end..]);
    std::mem::swap(entries, alternate);
}

fn is_descendant(compiled: &CompiledDefinition, state: StateId, ancestor: StateId) -> bool {
    if let Some(ends) = &compiled.history_subtree_ends {
        let state_order = compiled.state(state).document_order;
        let ancestor_order = compiled.state(ancestor).document_order;
        return state_order > ancestor_order && state_order < ends[ancestor.index()];
    }

    let mut cursor = compiled.state(state).parent;
    while let Some(current) = cursor {
        if current == ancestor {
            return true;
        }
        cursor = compiled.state(current).parent;
    }
    false
}

fn parallel_candidate_may_conflict(
    compiled: &CompiledDefinition,
    runtime: &ParallelRuntime,
    candidate: SelectedTransition,
) -> bool {
    if candidate.transition.target.is_none() || runtime.selected_targetful == 0 {
        return false;
    }
    let Some(domain) = candidate.transition.domain else {
        return true;
    };
    if runtime.selected_root_domain
        || runtime.selected_domains[domain.index()]
        || runtime.selected_domain_descendants[domain.index()] != 0
    {
        return true;
    }
    let mut cursor = compiled.state(domain).parent;
    while let Some(ancestor) = cursor {
        if runtime.selected_domains[ancestor.index()] {
            return true;
        }
        cursor = compiled.state(ancestor).parent;
    }
    false
}

fn mark_selected_transition(
    compiled: &CompiledDefinition,
    runtime: &mut ParallelRuntime,
    selected: SelectedTransition,
) {
    runtime.selected_sources[selected.source.index()] = true;
    if selected.transition.target.is_none() {
        return;
    }
    runtime.selected_targetful += 1;
    let Some(domain) = selected.transition.domain else {
        runtime.selected_root_domain = true;
        return;
    };
    runtime.selected_domains[domain.index()] = true;
    let mut cursor = compiled.state(domain).parent;
    while let Some(ancestor) = cursor {
        runtime.selected_domain_descendants[ancestor.index()] += 1;
        cursor = compiled.state(ancestor).parent;
    }
}

fn unmark_selected_transition(
    compiled: &CompiledDefinition,
    runtime: &mut ParallelRuntime,
    selected: SelectedTransition,
) {
    runtime.selected_sources[selected.source.index()] = runtime
        .selected
        .iter()
        .any(|candidate| candidate.source == selected.source);
    if selected.transition.target.is_none() {
        return;
    }
    runtime.selected_targetful -= 1;
    let Some(domain) = selected.transition.domain else {
        runtime.selected_root_domain = false;
        return;
    };
    runtime.selected_domains[domain.index()] = false;
    let mut cursor = compiled.state(domain).parent;
    while let Some(ancestor) = cursor {
        runtime.selected_domain_descendants[ancestor.index()] -= 1;
        cursor = compiled.state(ancestor).parent;
    }
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
    use std::{num::NonZeroUsize, time::Duration};

    use super::*;
    use crate::{
        StateTreeAction, StateTreeBuilder, StateTreeData, StateTreeTransition,
        test_harness::TestApp,
    };

    struct TestDomain;

    impl crate::StateTreeDomain for TestDomain {
        const AUTHORED_NAME: &'static str = "test";

        fn configure(config: &mut crate::StateTreeConfig<'_, Self>) {
            config.event::<Advance>().event::<Finish>();
        }
    }

    struct TestEvents;

    #[derive(Clone, Copy, bevy_ecs::event::Event, crate::StateTreeEvent)]
    #[state_tree(domain = TestDomain, name = "advance", broadcast)]
    struct Advance;

    #[derive(Clone, Copy, bevy_ecs::event::Event, crate::StateTreeEvent)]
    #[state_tree(domain = TestDomain, name = "finish", broadcast)]
    struct Finish;

    #[allow(non_upper_case_globals)]
    impl TestEvents {
        const Advance: Advance = Advance;
        const Finish: Finish = Finish;
    }

    fn instance(definition: &StateTreeDefinition, limit: usize) -> TestApp<TestDomain> {
        TestApp::new(
            definition.clone(),
            NonZeroUsize::new(limit).expect("test limit is non-zero"),
        )
    }

    fn process_event<E: crate::BroadcastStateTreeEvent<TestDomain> + Clone>(
        instance: &mut TestApp<TestDomain>,
        event: &E,
        limit: usize,
    ) -> bool {
        assert!(limit > 0);
        instance.process_event(event.clone())
    }

    fn execution(definition: &StateTreeDefinition) -> TestApp<TestDomain> {
        TestApp::new(definition.clone(), NonZeroUsize::new(64).unwrap())
    }

    #[test]
    fn initialized_definitions_without_activities_have_no_activity_slot_storage() {
        let mut builder = StateTreeBuilder::new("no-activities");
        let root = builder.compound_root("root").unwrap();
        let idle = builder.atomic("idle", root).unwrap();
        builder.initial(root, idle).unwrap();
        let definition = builder.build().unwrap();

        let instance = instance(&definition, 8);

        assert!(instance.instance().activities.is_none());
    }

    #[test]
    fn initial_data_rejects_duplicate_names() {
        let result = StateTreeDataOverrides::try_new([
            ("value", LuauValue::Integer(1)),
            ("value", LuauValue::Integer(2)),
        ]);

        assert_eq!(
            result.unwrap_err(),
            StateTreeDataOverrideError::DuplicateName {
                name: "value".into()
            }
        );
    }

    #[test]
    fn initial_data_overrides_authored_defaults_before_state_entry() {
        let mut builder = StateTreeBuilder::new("initial-data");
        builder
            .luau_datamodel([
                StateTreeData::new("seed").with_initializer("1"),
                StateTreeData::new("observed").with_initializer("0"),
            ])
            .unwrap();
        let root = builder.compound_root("root").unwrap();
        let active = builder.atomic("active", root).unwrap();
        builder.initial(root, active).unwrap();
        builder
            .entry_actions(active, [StateTreeAction::assign("observed", "seed")])
            .unwrap();
        let definition = builder.build().unwrap();
        let initial_data =
            StateTreeDataOverrides::try_new([("seed", LuauValue::Integer(7))]).unwrap();

        let execution = TestApp::<TestDomain>::with_initial_data(
            definition,
            NonZeroUsize::new(64).unwrap(),
            initial_data,
        );

        assert_eq!(execution.status(), &InstanceStatus::Running);
        assert_eq!(execution.data("seed"), Some(&LuauValue::Integer(7)));
        assert_eq!(execution.data("observed"), Some(&LuauValue::Integer(7)));
    }

    #[test]
    fn initial_data_is_retained_by_pending_bevy_instances() {
        let mut builder = StateTreeBuilder::new("bevy-initial-data");
        builder
            .luau_datamodel([StateTreeData::new("value").with_initializer("1")])
            .unwrap();
        let root = builder.compound_root("root").unwrap();
        let active = builder.atomic("active", root).unwrap();
        builder.initial(root, active).unwrap();
        let definition = builder.build().unwrap();
        let initial_data =
            StateTreeDataOverrides::try_new([("value", LuauValue::Integer(9))]).unwrap();
        let instance = TestApp::<TestDomain>::with_initial_data(
            definition,
            NonZeroUsize::new(64).unwrap(),
            initial_data,
        );

        assert_eq!(instance.status(), &InstanceStatus::Running);
        assert_eq!(instance.data("value"), Some(&LuauValue::Integer(9)));
    }

    #[test]
    fn unknown_initial_data_faults_before_entering_states() {
        let mut builder = StateTreeBuilder::new("unknown-initial-data");
        builder
            .luau_datamodel([StateTreeData::new("known").with_initializer("1")])
            .unwrap();
        let root = builder.compound_root("root").unwrap();
        let active = builder.atomic("active", root).unwrap();
        builder.initial(root, active).unwrap();
        let definition = builder.build().unwrap();
        let initial_data = StateTreeDataOverrides::try_new([
            ("known", LuauValue::Integer(2)),
            ("missing", LuauValue::Integer(3)),
        ])
        .unwrap();

        let execution = TestApp::<TestDomain>::with_initial_data(
            definition,
            NonZeroUsize::new(64).unwrap(),
            initial_data,
        );

        assert_eq!(
            execution.status(),
            &InstanceStatus::Faulted(RuntimeFault::UnknownDataOverride {
                name: "missing".into()
            })
        );
        assert_eq!(execution.active_state_names().next(), None);
        assert_eq!(execution.data("known"), Some(&LuauValue::Integer(1)));
    }

    #[test]
    fn initial_data_requires_a_luau_datamodel() {
        let mut builder = StateTreeBuilder::new("missing-datamodel");
        let root = builder.compound_root("root").unwrap();
        let active = builder.atomic("active", root).unwrap();
        builder.initial(root, active).unwrap();
        let definition = builder.build().unwrap();
        let initial_data =
            StateTreeDataOverrides::try_new([("value", LuauValue::Integer(1))]).unwrap();

        let execution = TestApp::<TestDomain>::with_initial_data(
            definition,
            NonZeroUsize::new(64).unwrap(),
            initial_data,
        );

        assert_eq!(
            execution.status(),
            &InstanceStatus::Faulted(RuntimeFault::UnknownDataOverride {
                name: "value".into()
            })
        );
        assert_eq!(execution.active_state_names().next(), None);
    }

    fn advance(execution: &mut TestApp<TestDomain>, duration: Duration) -> bool {
        execution.advance_time(duration)
    }

    struct DescriptorDomain;

    impl StateTreeDomain for DescriptorDomain {
        const AUTHORED_NAME: &'static str = "descriptor";

        fn configure(config: &mut crate::StateTreeConfig<'_, Self>) {
            config.event::<DescriptorEvents>();
        }
    }

    #[derive(Clone, Copy, bevy_ecs::event::Event, crate::StateTreeEvent)]
    #[state_tree(domain = DescriptorDomain, name = "combat.damage", broadcast)]
    struct DescriptorEvents;

    #[allow(non_upper_case_globals)]
    impl DescriptorEvents {
        const Combat: Self = Self;
    }

    #[test]
    fn linked_external_events_match_scxml_prefix_descriptors() {
        let mut builder = StateTreeBuilder::new("descriptors");
        let root = builder.compound_root("root").unwrap();
        let waiting = builder.atomic("waiting", root).unwrap();
        let done = builder.final_state("done", root).unwrap();
        builder.initial(root, waiting).unwrap();
        builder
            .add_transition(waiting, StateTreeTransition::on("combat").target(done))
            .unwrap();
        let definition = builder.build().unwrap();
        let mut execution =
            TestApp::<DescriptorDomain>::new(definition, NonZeroUsize::new(8).unwrap());

        assert!(execution.process_event(DescriptorEvents::Combat));
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
        let history = builder.shallow_history("left-history", left).unwrap();
        builder.history_default(history, left_leaf).unwrap();
        builder.initial(root, parallel).unwrap();
        builder.initial(left, left_leaf).unwrap();
        builder.initial(right, right_leaf).unwrap();
        let definition = builder.build().unwrap();

        assert!(is_descendant(&definition.compiled, left_leaf, left));
        assert!(!is_descendant(&definition.compiled, right_leaf, left));

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
    fn wide_parallel_regions_toggle_repeatedly_in_canonical_order() {
        const REGIONS: usize = 64;

        let mut builder = StateTreeBuilder::new("wide-parallel");
        let root = builder.compound_root("root").unwrap();
        let parallel = builder.parallel("parallel", root).unwrap();
        builder.initial(root, parallel).unwrap();
        for index in 0..REGIONS {
            let region = builder
                .compound(format!("region-{index}"), parallel)
                .unwrap();
            let left = builder
                .atomic(format!("region-{index}-left"), region)
                .unwrap();
            let right = builder
                .atomic(format!("region-{index}-right"), region)
                .unwrap();
            builder.initial(region, left).unwrap();
            builder
                .add_transition(left, StateTreeTransition::on("advance").target(right))
                .unwrap();
            builder
                .add_transition(right, StateTreeTransition::on("advance").target(left))
                .unwrap();
        }
        let definition = builder.build().unwrap();
        let mut instance = instance(&definition, 128);

        for expected_leaf in ["right", "left", "right"] {
            assert!(process_event(&mut instance, &TestEvents::Advance, 128));
            let active = instance.active_state_names().collect::<Vec<_>>();
            assert_eq!(active.len(), 2 + REGIONS * 2);
            for index in 0..REGIONS {
                assert_eq!(active[2 + index * 2], format!("region-{index}"));
                assert_eq!(
                    active[3 + index * 2],
                    format!("region-{index}-{expected_leaf}")
                );
            }
        }
    }

    #[test]
    fn shallow_history_restores_the_last_direct_child() {
        let mut builder = StateTreeBuilder::new("history");
        let root = builder.compound_root("root").unwrap();
        let owner = builder.compound("owner", root).unwrap();
        let first = builder.atomic("first", owner).unwrap();
        let second = builder.atomic("second", owner).unwrap();
        let outside = builder.atomic("outside", root).unwrap();
        let history = builder.shallow_history("recent", owner).unwrap();
        builder.history_default(history, first).unwrap();
        builder.initial(root, owner).unwrap();
        builder.initial(owner, first).unwrap();
        builder
            .add_transition(first, StateTreeTransition::on("advance").target(second))
            .unwrap();
        builder
            .add_transition(second, StateTreeTransition::on("finish").target(outside))
            .unwrap();
        builder
            .add_transition(outside, StateTreeTransition::on("advance").target(history))
            .unwrap();
        let definition = builder.build().unwrap();
        let mut instance = instance(&definition, 64);

        process_event(&mut instance, &TestEvents::Advance, 64);
        process_event(&mut instance, &TestEvents::Finish, 64);
        process_event(&mut instance, &TestEvents::Advance, 64);

        assert_eq!(
            instance.active_state_names().collect::<Vec<_>>(),
            ["root", "owner", "second"]
        );
    }

    #[test]
    fn initial_history_target_uses_default_then_stored_child() {
        let mut builder = StateTreeBuilder::new("initial-history");
        let root = builder.compound_root("root").unwrap();
        let owner = builder.compound("owner", root).unwrap();
        let first = builder.atomic("first", owner).unwrap();
        let second = builder.atomic("second", owner).unwrap();
        let outside = builder.atomic("outside", root).unwrap();
        let history = builder.shallow_history("recent", owner).unwrap();
        builder.history_default(history, first).unwrap();
        builder.initial(root, owner).unwrap();
        builder.initial(owner, history).unwrap();
        builder
            .add_transition(first, StateTreeTransition::on("advance").target(second))
            .unwrap();
        builder
            .add_transition(second, StateTreeTransition::on("finish").target(outside))
            .unwrap();
        builder
            .add_transition(outside, StateTreeTransition::on("advance").target(owner))
            .unwrap();
        let definition = builder.build().unwrap();
        let mut instance = instance(&definition, 64);
        assert_eq!(
            instance.active_state_names().collect::<Vec<_>>(),
            ["root", "owner", "first"]
        );

        process_event(&mut instance, &TestEvents::Advance, 64);
        process_event(&mut instance, &TestEvents::Finish, 64);
        process_event(&mut instance, &TestEvents::Advance, 64);

        assert_eq!(
            instance.active_state_names().collect::<Vec<_>>(),
            ["root", "owner", "second"]
        );
    }

    #[test]
    fn shallow_history_restores_a_nested_child_at_its_initial_leaf() {
        let mut builder = StateTreeBuilder::new("nested-shallow-history");
        let root = builder.compound_root("root").unwrap();
        let owner = builder.compound("owner", root).unwrap();
        let nested = builder.compound("nested", owner).unwrap();
        let initial_leaf = builder.atomic("initial-leaf", nested).unwrap();
        let prior_leaf = builder.atomic("prior-leaf", nested).unwrap();
        let outside = builder.atomic("outside", root).unwrap();
        let history = builder.shallow_history("recent", owner).unwrap();
        builder.history_default(history, nested).unwrap();
        builder.initial(root, owner).unwrap();
        builder.initial(owner, nested).unwrap();
        builder.initial(nested, initial_leaf).unwrap();
        builder
            .add_transition(
                initial_leaf,
                StateTreeTransition::on("advance").target(prior_leaf),
            )
            .unwrap();
        builder
            .add_transition(
                prior_leaf,
                StateTreeTransition::on("finish").target(outside),
            )
            .unwrap();
        builder
            .add_transition(outside, StateTreeTransition::on("advance").target(history))
            .unwrap();
        let definition = builder.build().unwrap();
        let mut instance = instance(&definition, 64);

        process_event(&mut instance, &TestEvents::Advance, 64);
        process_event(&mut instance, &TestEvents::Finish, 64);
        process_event(&mut instance, &TestEvents::Advance, 64);

        assert_eq!(
            instance.active_state_names().collect::<Vec<_>>(),
            ["root", "owner", "nested", "initial-leaf"]
        );
    }

    #[test]
    fn history_default_actions_follow_owner_initial_actions_and_do_not_run_when_stored() {
        let mut builder = StateTreeBuilder::new("history-action-order");
        builder
            .luau_datamodel([StateTreeData::new("log").with_initializer("''")])
            .unwrap();
        let root = builder.compound_root("root").unwrap();
        let owner = builder.compound("owner", root).unwrap();
        let first = builder.atomic("first", owner).unwrap();
        let second = builder.atomic("second", owner).unwrap();
        let outside = builder.atomic("outside", root).unwrap();
        let history = builder.shallow_history("recent", owner).unwrap();
        builder.history_default(history, first).unwrap();
        builder
            .history_actions(
                history,
                [StateTreeAction::assign("log", "log .. 'history>'")],
            )
            .unwrap();
        builder.initial(root, owner).unwrap();
        builder.initial(owner, history).unwrap();
        builder
            .entry_actions(owner, [StateTreeAction::assign("log", "log .. 'owner>'")])
            .unwrap();
        builder
            .initial_actions(owner, [StateTreeAction::assign("log", "log .. 'initial>'")])
            .unwrap();
        builder
            .entry_actions(first, [StateTreeAction::assign("log", "log .. 'first>'")])
            .unwrap();
        builder
            .entry_actions(second, [StateTreeAction::assign("log", "log .. 'second>'")])
            .unwrap();
        builder
            .add_transition(first, StateTreeTransition::on("advance").target(second))
            .unwrap();
        builder
            .add_transition(second, StateTreeTransition::on("finish").target(outside))
            .unwrap();
        builder
            .add_transition(outside, StateTreeTransition::on("advance").target(history))
            .unwrap();
        let definition = builder.build().unwrap();
        let mut instance = instance(&definition, 64);

        assert_eq!(
            instance.data("log"),
            Some(&LuauValue::String("owner>initial>history>first>".into()))
        );

        process_event(&mut instance, &TestEvents::Advance, 64);
        process_event(&mut instance, &TestEvents::Finish, 64);
        process_event(&mut instance, &TestEvents::Advance, 64);

        assert_eq!(
            instance.data("log"),
            Some(&LuauValue::String(
                "owner>initial>history>first>second>owner>second>".into()
            ))
        );
    }

    #[test]
    fn repeated_owner_exits_replace_the_shallow_history_child() {
        let mut builder = StateTreeBuilder::new("history-overwrite");
        let root = builder.compound_root("root").unwrap();
        let owner = builder.compound("owner", root).unwrap();
        let first = builder.atomic("first", owner).unwrap();
        let second = builder.atomic("second", owner).unwrap();
        let outside = builder.atomic("outside", root).unwrap();
        let history = builder.shallow_history("recent", owner).unwrap();
        builder.history_default(history, first).unwrap();
        builder.initial(root, owner).unwrap();
        builder.initial(owner, first).unwrap();
        builder
            .add_transition(first, StateTreeTransition::on("advance").target(second))
            .unwrap();
        builder
            .add_transition(first, StateTreeTransition::on("finish").target(outside))
            .unwrap();
        builder
            .add_transition(second, StateTreeTransition::on("advance").target(first))
            .unwrap();
        builder
            .add_transition(second, StateTreeTransition::on("finish").target(outside))
            .unwrap();
        builder
            .add_transition(outside, StateTreeTransition::on("advance").target(history))
            .unwrap();
        let definition = builder.build().unwrap();
        let mut instance = instance(&definition, 64);

        process_event(&mut instance, &TestEvents::Advance, 64);
        process_event(&mut instance, &TestEvents::Finish, 64);
        process_event(&mut instance, &TestEvents::Advance, 64);
        assert_eq!(instance.active_state_names().last(), Some("second"));

        process_event(&mut instance, &TestEvents::Advance, 64);
        process_event(&mut instance, &TestEvents::Finish, 64);
        process_event(&mut instance, &TestEvents::Advance, 64);
        assert_eq!(instance.active_state_names().last(), Some("first"));
    }

    #[test]
    fn internal_history_transition_preserves_owner_for_default_and_stored_resolution() {
        let mut builder = StateTreeBuilder::new("internal-history");
        builder
            .luau_datamodel([
                StateTreeData::new("owner_entries").with_initializer("0"),
                StateTreeData::new("owner_exits").with_initializer("0"),
                StateTreeData::new("history_defaults").with_initializer("0"),
            ])
            .unwrap();
        let root = builder.compound_root("root").unwrap();
        let owner = builder.compound("owner", root).unwrap();
        let first = builder.atomic("first", owner).unwrap();
        let second = builder.atomic("second", owner).unwrap();
        let outside = builder.atomic("outside", root).unwrap();
        let history = builder.shallow_history("recent", owner).unwrap();
        builder.history_default(history, second).unwrap();
        builder
            .history_actions(
                history,
                [StateTreeAction::assign(
                    "history_defaults",
                    "history_defaults + 1",
                )],
            )
            .unwrap();
        builder.initial(root, owner).unwrap();
        builder.initial(owner, first).unwrap();
        builder
            .entry_actions(
                owner,
                [StateTreeAction::assign(
                    "owner_entries",
                    "owner_entries + 1",
                )],
            )
            .unwrap();
        builder
            .exit_actions(
                owner,
                [StateTreeAction::assign("owner_exits", "owner_exits + 1")],
            )
            .unwrap();
        builder
            .add_transition(
                owner,
                StateTreeTransition::on("finish").target(history).internal(),
            )
            .unwrap();
        builder
            .add_transition(second, StateTreeTransition::on("advance").target(outside))
            .unwrap();
        builder
            .add_transition(outside, StateTreeTransition::on("advance").target(owner))
            .unwrap();
        let definition = builder.build().unwrap();
        let mut instance = instance(&definition, 64);

        process_event(&mut instance, &TestEvents::Finish, 64);
        assert_eq!(instance.active_state_names().last(), Some("second"));
        assert_eq!(instance.data("owner_entries"), Some(&LuauValue::Integer(1)));
        assert_eq!(instance.data("owner_exits"), Some(&LuauValue::Integer(0)));
        assert_eq!(
            instance.data("history_defaults"),
            Some(&LuauValue::Integer(0))
        );

        process_event(&mut instance, &TestEvents::Advance, 64);
        process_event(&mut instance, &TestEvents::Advance, 64);
        assert_eq!(instance.active_state_names().last(), Some("first"));
        process_event(&mut instance, &TestEvents::Finish, 64);

        assert_eq!(instance.active_state_names().last(), Some("second"));
        assert_eq!(instance.data("owner_entries"), Some(&LuauValue::Integer(2)));
        assert_eq!(instance.data("owner_exits"), Some(&LuauValue::Integer(1)));
    }

    #[test]
    fn parallel_shallow_history_restarts_each_regions_initial_descendants() {
        let mut builder = StateTreeBuilder::new("parallel-history");
        let root = builder.compound_root("root").unwrap();
        let owner = builder.parallel("owner", root).unwrap();
        let left = builder.compound("left", owner).unwrap();
        let left_initial = builder.atomic("left-initial", left).unwrap();
        let left_prior = builder.atomic("left-prior", left).unwrap();
        let right = builder.compound("right", owner).unwrap();
        let right_initial = builder.atomic("right-initial", right).unwrap();
        let right_prior = builder.atomic("right-prior", right).unwrap();
        let outside = builder.atomic("outside", root).unwrap();
        let history = builder.shallow_history("recent", owner).unwrap();
        builder.history_default(history, left).unwrap();
        builder.initial(root, owner).unwrap();
        builder.initial(left, left_initial).unwrap();
        builder.initial(right, right_initial).unwrap();
        builder
            .add_transition(
                left_initial,
                StateTreeTransition::on("advance").target(left_prior),
            )
            .unwrap();
        builder
            .add_transition(
                right_initial,
                StateTreeTransition::on("advance").target(right_prior),
            )
            .unwrap();
        builder
            .add_transition(owner, StateTreeTransition::on("finish").target(outside))
            .unwrap();
        builder
            .add_transition(outside, StateTreeTransition::on("advance").target(history))
            .unwrap();
        let definition = builder.build().unwrap();
        let mut instance = instance(&definition, 64);

        process_event(&mut instance, &TestEvents::Advance, 64);
        process_event(&mut instance, &TestEvents::Finish, 64);
        process_event(&mut instance, &TestEvents::Advance, 64);

        assert_eq!(
            instance.active_state_names().collect::<Vec<_>>(),
            [
                "root",
                "owner",
                "left",
                "left-initial",
                "right",
                "right-initial"
            ]
        );
    }

    #[test]
    fn histories_with_one_owner_have_distinct_defaults_and_share_the_captured_child() {
        let mut builder = StateTreeBuilder::new("shared-history-slot");
        let root = builder.compound_root("root").unwrap();
        let owner = builder.compound("owner", root).unwrap();
        let first = builder.atomic("first", owner).unwrap();
        let second = builder.atomic("second", owner).unwrap();
        let outside = builder.atomic("outside", root).unwrap();
        let first_history = builder.shallow_history("first-history", owner).unwrap();
        let second_history = builder.shallow_history("second-history", owner).unwrap();
        builder.history_default(first_history, first).unwrap();
        builder.history_default(second_history, second).unwrap();
        builder.initial(root, owner).unwrap();
        builder.initial(owner, first_history).unwrap();
        builder
            .add_transition(
                owner,
                StateTreeTransition::on("advance")
                    .target(second_history)
                    .internal(),
            )
            .unwrap();
        builder
            .add_transition(second, StateTreeTransition::on("finish").target(outside))
            .unwrap();
        builder
            .add_transition(
                outside,
                StateTreeTransition::on("advance").target(first_history),
            )
            .unwrap();
        let definition = builder.build().unwrap();
        let mut instance = instance(&definition, 64);

        assert_eq!(instance.active_state_names().last(), Some("first"));
        process_event(&mut instance, &TestEvents::Advance, 64);
        assert_eq!(instance.active_state_names().last(), Some("second"));
        process_event(&mut instance, &TestEvents::Finish, 64);
        process_event(&mut instance, &TestEvents::Advance, 64);

        assert_eq!(instance.active_state_names().last(), Some("second"));
        assert!(
            !instance
                .active_state_names()
                .any(|state| state == "first-history" || state == "second-history")
        );
    }

    #[test]
    fn history_trace_identifies_the_target_and_default_and_stored_resolutions() {
        let mut builder = StateTreeBuilder::new("history-trace");
        let root = builder.compound_root("root").unwrap();
        let owner = builder.compound("owner", root).unwrap();
        let first = builder.atomic("first", owner).unwrap();
        let outside = builder.atomic("outside", root).unwrap();
        let history = builder.shallow_history("recent", owner).unwrap();
        builder.history_default(history, first).unwrap();
        builder.initial(root, outside).unwrap();
        builder.initial(owner, first).unwrap();
        builder
            .add_transition(outside, StateTreeTransition::on("advance").target(history))
            .unwrap();
        builder
            .add_transition(first, StateTreeTransition::on("finish").target(outside))
            .unwrap();
        let definition = builder.build().unwrap();
        let mut execution = execution(&definition);
        execution.process_event(TestEvents::Advance);
        execution.process_event(TestEvents::Finish);
        execution.process_event(TestEvents::Advance);
        let trace = execution.trace();

        assert_eq!(
            trace
                .as_str()
                .matches("target history \"recent\" owner \"owner\"\n")
                .count(),
            2
        );
        assert_eq!(
            trace
                .as_str()
                .matches("history \"recent\" default\n")
                .count(),
            1
        );
        assert_eq!(
            trace
                .as_str()
                .matches("history \"recent\" stored\n")
                .count(),
            1
        );
        assert!(
            !execution
                .active_state_names()
                .any(|state| state == "recent")
        );
    }

    #[test]
    fn shallow_history_can_restore_a_final_child_without_completing_the_root() {
        let mut builder = StateTreeBuilder::new("history-final");
        let root = builder.compound_root("root").unwrap();
        let owner = builder.compound("owner", root).unwrap();
        let running = builder.atomic("running", owner).unwrap();
        let done = builder.final_state("done", owner).unwrap();
        let outside = builder.atomic("outside", root).unwrap();
        let history = builder.shallow_history("recent", owner).unwrap();
        builder.history_default(history, running).unwrap();
        builder.initial(root, owner).unwrap();
        builder.initial(owner, running).unwrap();
        builder
            .add_transition(running, StateTreeTransition::on("finish").target(done))
            .unwrap();
        builder
            .add_transition(owner, StateTreeTransition::on("advance").target(outside))
            .unwrap();
        builder
            .add_transition(outside, StateTreeTransition::on("advance").target(history))
            .unwrap();
        let definition = builder.build().unwrap();
        let mut instance = instance(&definition, 64);

        process_event(&mut instance, &TestEvents::Finish, 64);
        process_event(&mut instance, &TestEvents::Advance, 64);
        process_event(&mut instance, &TestEvents::Advance, 64);

        assert_eq!(instance.status(), &InstanceStatus::Running);
        assert_eq!(
            instance.active_state_names().collect::<Vec<_>>(),
            ["root", "owner", "done"]
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
        builder
            .add_transition(
                root,
                StateTreeTransition::on("advance").target(parent_target),
            )
            .unwrap();
        builder
            .add_transition(leaf, StateTreeTransition::on("advance").target(leaf_target))
            .unwrap();
        let definition = builder.build().unwrap();
        let mut instance = instance(&definition, 64);

        process_event(&mut instance, &TestEvents::Advance, 64);

        assert_eq!(
            instance.active_state_names().collect::<Vec<_>>(),
            ["root", "leaf-target"]
        );
    }

    #[test]
    fn external_self_transition_exits_and_reenters_its_source() {
        let mut builder = StateTreeBuilder::new("external-self-transition");
        builder
            .luau_datamodel([
                StateTreeData::new("entries").with_initializer("0"),
                StateTreeData::new("exits").with_initializer("0"),
            ])
            .unwrap();
        let root = builder.compound_root("root").unwrap();
        let active = builder.atomic("active", root).unwrap();
        builder.initial(root, active).unwrap();
        builder
            .entry_actions(active, [StateTreeAction::assign("entries", "entries + 1")])
            .unwrap();
        builder
            .exit_actions(active, [StateTreeAction::assign("exits", "exits + 1")])
            .unwrap();
        builder
            .add_transition(active, StateTreeTransition::on("advance").target(active))
            .unwrap();
        let definition = builder.build().unwrap();
        let mut instance = instance(&definition, 64);

        process_event(&mut instance, &TestEvents::Advance, 64);

        assert_eq!(
            instance.active_state_names().collect::<Vec<_>>(),
            ["root", "active"]
        );
        assert_eq!(instance.data("entries"), Some(&LuauValue::Integer(2)));
        assert_eq!(instance.data("exits"), Some(&LuauValue::Integer(1)));
    }

    #[test]
    fn external_ancestor_transition_reenters_its_initial_configuration() {
        let mut builder = StateTreeBuilder::new("external-ancestor-transition");
        builder
            .luau_datamodel([
                StateTreeData::new("parent_entries").with_initializer("0"),
                StateTreeData::new("parent_exits").with_initializer("0"),
                StateTreeData::new("child_entries").with_initializer("0"),
                StateTreeData::new("child_exits").with_initializer("0"),
            ])
            .unwrap();
        let root = builder.compound_root("root").unwrap();
        let parent = builder.compound("parent", root).unwrap();
        let child = builder.atomic("child", parent).unwrap();
        builder.initial(root, parent).unwrap();
        builder.initial(parent, child).unwrap();
        builder
            .entry_actions(
                parent,
                [StateTreeAction::assign(
                    "parent_entries",
                    "parent_entries + 1",
                )],
            )
            .unwrap();
        builder
            .exit_actions(
                parent,
                [StateTreeAction::assign("parent_exits", "parent_exits + 1")],
            )
            .unwrap();
        builder
            .entry_actions(
                child,
                [StateTreeAction::assign(
                    "child_entries",
                    "child_entries + 1",
                )],
            )
            .unwrap();
        builder
            .exit_actions(
                child,
                [StateTreeAction::assign("child_exits", "child_exits + 1")],
            )
            .unwrap();
        builder
            .add_transition(child, StateTreeTransition::on("advance").target(parent))
            .unwrap();
        let definition = builder.build().unwrap();
        let mut instance = instance(&definition, 64);

        process_event(&mut instance, &TestEvents::Advance, 64);

        assert_eq!(
            instance.active_state_names().collect::<Vec<_>>(),
            ["root", "parent", "child"]
        );
        assert_eq!(
            instance.data("parent_entries"),
            Some(&LuauValue::Integer(2))
        );
        assert_eq!(instance.data("parent_exits"), Some(&LuauValue::Integer(1)));
        assert_eq!(instance.data("child_entries"), Some(&LuauValue::Integer(2)));
        assert_eq!(instance.data("child_exits"), Some(&LuauValue::Integer(1)));
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
        builder
            .add_transition(idle, StateTreeTransition::on("advance").target(first))
            .unwrap();
        builder
            .add_transition(first, StateTreeTransition::eventless().target(second))
            .unwrap();
        builder
            .add_transition(second, StateTreeTransition::eventless().target(stable))
            .unwrap();
        let definition = builder.build().unwrap();
        let mut instance = instance(&definition, 3);

        process_event(&mut instance, &TestEvents::Advance, 3);

        assert_eq!(instance.status(), &InstanceStatus::Running);
        assert_eq!(
            instance.active_state_names().collect::<Vec<_>>(),
            ["root", "stable"]
        );
    }

    #[test]
    fn eventless_transition_precedes_a_queued_internal_event() {
        let mut builder = StateTreeBuilder::new("eventless-before-internal");
        let root = builder.compound_root("root").unwrap();
        let idle = builder.atomic("idle", root).unwrap();
        let choice = builder.atomic("choice", root).unwrap();
        let eventless_winner = builder.atomic("eventless-winner", root).unwrap();
        let internal_winner = builder.atomic("internal-winner", root).unwrap();
        builder.initial(root, idle).unwrap();
        builder
            .add_transition(
                idle,
                StateTreeTransition::on("advance")
                    .target(choice)
                    .actions([StateTreeAction::raise("continue")]),
            )
            .unwrap();
        builder
            .add_transition(
                choice,
                StateTreeTransition::eventless().target(eventless_winner),
            )
            .unwrap();
        builder
            .add_transition(
                choice,
                StateTreeTransition::on("continue").target(internal_winner),
            )
            .unwrap();
        let definition = builder.build().unwrap();
        let mut instance = instance(&definition, 64);

        process_event(&mut instance, &TestEvents::Advance, 64);

        assert_eq!(
            instance.active_state_names().collect::<Vec<_>>(),
            ["root", "eventless-winner"]
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
        builder
            .add_transition(idle, StateTreeTransition::on("advance").target(first))
            .unwrap();
        builder
            .add_transition(first, StateTreeTransition::eventless().target(stable))
            .unwrap();
        let definition = builder.build().unwrap();
        let mut instance = instance(&definition, 1);

        process_event(&mut instance, &TestEvents::Advance, 1);

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
        builder
            .add_transition(idle, StateTreeTransition::on("advance").target(stable))
            .unwrap();
        let definition = builder.build().unwrap();
        let mut instance = instance(&definition, 1);

        process_event(&mut instance, &TestEvents::Advance, 1);

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
        builder
            .add_transition(first, StateTreeTransition::eventless().target(stable))
            .unwrap();
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
        builder
            .add_transition(first, StateTreeTransition::eventless().target(second))
            .unwrap();
        builder
            .add_transition(second, StateTreeTransition::eventless().target(first))
            .unwrap();
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
        builder
            .add_transition(running, StateTreeTransition::on("finish").target(done))
            .unwrap();
        let definition = builder.build().unwrap();
        let mut instance = instance(&definition, 64);

        process_event(&mut instance, &TestEvents::Finish, 64);

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
        builder
            .add_transition(running, StateTreeTransition::on("finish").target(done))
            .unwrap();
        let definition = builder.build().unwrap();
        let mut instance = instance(&definition, 64);

        process_event(&mut instance, &TestEvents::Finish, 64);

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
        builder
            .add_transition(
                running,
                StateTreeTransition::on("finish").target(nested_done),
            )
            .unwrap();
        builder
            .add_transition(
                nested,
                StateTreeTransition::on("done.state.nested").target(complete),
            )
            .unwrap();
        let definition = builder.build().unwrap();
        let mut instance = instance(&definition, 64);

        process_event(&mut instance, &TestEvents::Finish, 64);

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
            .add_transition(
                nested,
                StateTreeTransition::on("done.state.nested").target(stable),
            )
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
        builder
            .add_transition(running, StateTreeTransition::on("finish").target(done))
            .unwrap();
        builder
            .add_transition(nested, StateTreeTransition::on("done.state.nested"))
            .unwrap();
        let definition = builder.build().unwrap();
        let mut instance = instance(&definition, 3);

        process_event(&mut instance, &TestEvents::Finish, 3);

        assert_eq!(instance.status(), &InstanceStatus::Running);
        assert_eq!(
            instance.active_state_names().collect::<Vec<_>>(),
            ["root", "nested", "done"]
        );
    }

    #[test]
    fn app_after_transition_fires_only_at_its_deadline() {
        let mut builder = StateTreeBuilder::new("deadline");
        let root = builder.compound_root("root").unwrap();
        let waiting = builder.atomic("waiting", root).unwrap();
        let elapsed = builder.atomic("elapsed", root).unwrap();
        builder.initial(root, waiting).unwrap();
        builder
            .add_transition(
                waiting,
                StateTreeTransition::after(Duration::from_secs(1)).target(elapsed),
            )
            .unwrap();
        let mut execution = execution(&builder.build().unwrap());

        assert!(!advance(&mut execution, Duration::from_millis(999)));
        assert_eq!(execution.active_state_names().last(), Some("waiting"));
        assert!(advance(&mut execution, Duration::from_millis(1)));
        assert_eq!(execution.active_state_names().last(), Some("elapsed"));
    }

    #[test]
    fn timer_rearm_beyond_the_clock_range_does_not_refire_immediately() {
        let mut builder = StateTreeBuilder::new("maximum-deadline");
        let root = builder.compound_root("root").unwrap();
        let waiting = builder.atomic("waiting", root).unwrap();
        builder.initial(root, waiting).unwrap();
        builder
            .add_transition(
                waiting,
                StateTreeTransition::after(Duration::MAX).target(waiting),
            )
            .unwrap();
        let mut execution = execution(&builder.build().unwrap());

        assert!(advance(&mut execution, Duration::MAX));
        assert!(!advance(&mut execution, Duration::ZERO));
        assert_eq!(execution.active_state_names().last(), Some("waiting"));
    }

    #[test]
    fn exiting_cancels_a_timeout_and_reentry_rearms_it() {
        let mut builder = StateTreeBuilder::new("rearm");
        let root = builder.compound_root("root").unwrap();
        let waiting = builder.atomic("waiting", root).unwrap();
        let away = builder.atomic("away", root).unwrap();
        let elapsed = builder.atomic("elapsed", root).unwrap();
        builder.initial(root, waiting).unwrap();
        builder
            .add_transition(
                waiting,
                StateTreeTransition::after(Duration::from_secs(1)).target(elapsed),
            )
            .unwrap();
        builder
            .add_transition(waiting, StateTreeTransition::on("advance").target(away))
            .unwrap();
        builder
            .add_transition(away, StateTreeTransition::on("finish").target(waiting))
            .unwrap();
        let mut execution = execution(&builder.build().unwrap());
        advance(&mut execution, Duration::from_millis(500));
        execution.process_event(TestEvents::Advance);
        assert!(!advance(&mut execution, Duration::from_secs(1)));
        execution.process_event(TestEvents::Finish);
        assert!(!advance(&mut execution, Duration::from_millis(999)));
        assert_eq!(execution.active_state_names().last(), Some("waiting"));
        assert!(advance(&mut execution, Duration::from_millis(1)));
        assert_eq!(execution.active_state_names().last(), Some("elapsed"));
    }

    #[test]
    fn targetless_after_transition_fires_once() {
        let mut builder = StateTreeBuilder::new("targetless-timeout");
        builder
            .luau_datamodel([StateTreeData::new("fires").with_initializer("0")])
            .unwrap();
        let root = builder.compound_root("root").unwrap();
        let waiting = builder.atomic("waiting", root).unwrap();
        builder.initial(root, waiting).unwrap();
        builder
            .add_transition(
                waiting,
                StateTreeTransition::after(Duration::from_millis(10))
                    .actions([StateTreeAction::assign("fires", "fires + 1")]),
            )
            .unwrap();
        let mut execution = execution(&builder.build().unwrap());

        assert!(advance(&mut execution, Duration::from_millis(10)));
        assert_eq!(execution.data("fires"), Some(&LuauValue::Integer(1)));
        assert!(!advance(&mut execution, Duration::from_secs(10)));
        assert_eq!(execution.data("fires"), Some(&LuauValue::Integer(1)));
    }

    #[test]
    fn same_deadline_after_transitions_use_authored_guard_order() {
        let mut builder = StateTreeBuilder::new("guarded-timeouts");
        builder.luau_datamodel([]).unwrap();
        let root = builder.compound_root("root").unwrap();
        let waiting = builder.atomic("waiting", root).unwrap();
        let rejected = builder.atomic("rejected", root).unwrap();
        let accepted = builder.atomic("accepted", root).unwrap();
        builder.initial(root, waiting).unwrap();
        builder
            .add_transition(
                waiting,
                StateTreeTransition::after(Duration::from_secs(1))
                    .condition("false")
                    .target(rejected),
            )
            .unwrap();
        builder
            .add_transition(
                waiting,
                StateTreeTransition::after(Duration::from_secs(1))
                    .condition("true")
                    .target(accepted),
            )
            .unwrap();
        let mut execution = execution(&builder.build().unwrap());

        assert!(advance(&mut execution, Duration::from_secs(1)));
        assert_eq!(execution.active_state_names().last(), Some("accepted"));
    }

    #[test]
    fn overdue_deadlines_run_chronologically_and_cancel_later_timeouts() {
        let mut builder = StateTreeBuilder::new("overdue-timeouts");
        let root = builder.compound_root("root").unwrap();
        let waiting = builder.atomic("waiting", root).unwrap();
        let first = builder.atomic("first", root).unwrap();
        let wrong = builder.atomic("wrong", root).unwrap();
        builder.initial(root, waiting).unwrap();
        builder
            .add_transition(
                waiting,
                StateTreeTransition::after(Duration::from_secs(2)).target(wrong),
            )
            .unwrap();
        builder
            .add_transition(
                waiting,
                StateTreeTransition::after(Duration::from_secs(1)).target(first),
            )
            .unwrap();
        let mut execution = execution(&builder.build().unwrap());

        assert!(advance(&mut execution, Duration::from_secs(3)));
        assert_eq!(execution.active_state_names().last(), Some("first"));
    }

    #[test]
    fn parallel_same_deadline_timeouts_form_one_cohort() {
        let mut builder = StateTreeBuilder::new("parallel-timeouts");
        let root = builder.compound_root("root").unwrap();
        let parallel = builder.parallel("parallel", root).unwrap();
        let left = builder.compound("left", parallel).unwrap();
        let left_waiting = builder.atomic("left-waiting", left).unwrap();
        let left_elapsed = builder.atomic("left-elapsed", left).unwrap();
        let right = builder.compound("right", parallel).unwrap();
        let right_waiting = builder.atomic("right-waiting", right).unwrap();
        let right_elapsed = builder.atomic("right-elapsed", right).unwrap();
        builder.initial(root, parallel).unwrap();
        builder.initial(left, left_waiting).unwrap();
        builder.initial(right, right_waiting).unwrap();
        builder
            .add_transition(
                left_waiting,
                StateTreeTransition::after(Duration::from_secs(1)).target(left_elapsed),
            )
            .unwrap();
        builder
            .add_transition(
                right_waiting,
                StateTreeTransition::after(Duration::from_secs(1)).target(right_elapsed),
            )
            .unwrap();
        let definition = builder.build().unwrap();
        let mut execution = execution(&definition);
        assert!(execution.advance_time(Duration::from_secs(1)));
        let active = execution.active_state_names().collect::<Vec<_>>();
        assert!(active.contains(&"left-elapsed"));
        assert!(active.contains(&"right-elapsed"));
        let trace = execution.trace();
        assert_eq!(trace.as_str().matches("macrostep timer\n").count(), 1);
        assert!(trace.as_str().contains("transition \"left-waiting\"\n"));
        assert!(trace.as_str().contains("transition \"right-waiting\"\n"));
    }

    #[test]
    fn timeout_macrostep_stabilizes_eventless_and_internal_transitions() {
        let mut builder = StateTreeBuilder::new("timeout-stabilization");
        let root = builder.compound_root("root").unwrap();
        let waiting = builder.atomic("waiting", root).unwrap();
        let eventless = builder.atomic("eventless", root).unwrap();
        let internal = builder.atomic("internal", root).unwrap();
        let stable = builder.atomic("stable", root).unwrap();
        builder.initial(root, waiting).unwrap();
        builder
            .add_transition(
                waiting,
                StateTreeTransition::after(Duration::from_secs(1))
                    .target(eventless)
                    .actions([StateTreeAction::raise("continue")]),
            )
            .unwrap();
        builder
            .add_transition(eventless, StateTreeTransition::eventless().target(internal))
            .unwrap();
        builder
            .add_transition(internal, StateTreeTransition::on("continue").target(stable))
            .unwrap();
        let mut execution = execution(&builder.build().unwrap());

        assert!(advance(&mut execution, Duration::from_secs(1)));
        assert_eq!(execution.active_state_names().last(), Some("stable"));
    }

    #[test]
    fn timeout_can_complete_or_fault_the_root_execution() {
        let mut complete_builder = StateTreeBuilder::new("timeout-complete");
        let root = complete_builder.compound_root("root").unwrap();
        let waiting = complete_builder.atomic("waiting", root).unwrap();
        let done = complete_builder.final_state("done", root).unwrap();
        complete_builder.initial(root, waiting).unwrap();
        complete_builder
            .add_transition(
                waiting,
                StateTreeTransition::after(Duration::from_secs(1)).target(done),
            )
            .unwrap();
        let mut complete = execution(&complete_builder.build().unwrap());

        assert!(advance(&mut complete, Duration::from_secs(1)));
        assert_eq!(complete.status(), &InstanceStatus::Complete);
        assert!(!advance(&mut complete, Duration::from_secs(1)));

        let mut fault_builder = StateTreeBuilder::new("timeout-fault");
        let root = fault_builder.compound_root("root").unwrap();
        let waiting = fault_builder.atomic("waiting", root).unwrap();
        let first = fault_builder.atomic("first", root).unwrap();
        let second = fault_builder.atomic("second", root).unwrap();
        fault_builder.initial(root, waiting).unwrap();
        fault_builder
            .add_transition(
                waiting,
                StateTreeTransition::after(Duration::from_secs(1)).target(first),
            )
            .unwrap();
        fault_builder
            .add_transition(first, StateTreeTransition::eventless().target(second))
            .unwrap();
        fault_builder
            .add_transition(second, StateTreeTransition::eventless().target(first))
            .unwrap();
        let definition = fault_builder.build().unwrap();
        let mut fault = TestApp::<TestDomain>::new(definition, NonZeroUsize::new(2).unwrap());

        assert!(fault.advance_time(Duration::from_secs(1)));
        assert_eq!(
            fault.status(),
            &InstanceStatus::Faulted(RuntimeFault::MicrostepLimitExceeded { limit: 2 })
        );
        assert!(!fault.advance_time(Duration::from_secs(1)));
    }
}
