use std::{
    collections::VecDeque,
    marker::PhantomData,
    num::NonZeroUsize,
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
    ErasedActivityToken, HeadlessActivitySink, RejectActivities, RuntimeActivitySink,
    RuntimeActivityStart, StateTreeActivityInbox, StateTreeActivitySink,
    StateTreeHeadlessActivityToken,
};
use crate::definition::{
    Action, ActivityId, CompiledDefinition, Entry, EventId, EventSet, ExpressionId,
    LinkedDefinition, StateId, StateKind, TimerId, Transition, TransitionTrigger,
};
use crate::effect::{
    HeadlessEffectSink, RejectEffects, RuntimeEffectSink, StateTreeEffectArgs,
    StateTreeEffectContext, StateTreeEffectSink,
};
use crate::{
    LuauValue, NoStateTreeTrace, StateTreeActivityArgs, StateTreeDefinition, StateTreeDomain,
    StateTreeEvents, StateTreeLinkError, StateTreeTrace, StateTreeTraceRecord,
    StateTreeTraceTrigger, luau,
    plugin::{StateTreeEventInbox, StateTreeTimers},
};

static NEXT_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);

enum Trigger<'a, E> {
    Eventless(Option<&'a str>, Option<&'a E>, Option<&'a LuauValue>),
    Internal(EventId, Option<&'a LuauValue>),
    External(&'a EventSet, &'a str, &'a E),
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
    event_data: Option<&'a LuauValue>,
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
            Self::Eventless(name, _, _) => *name,
            Self::Internal(event, _) => compiled.event_name(*event),
            Self::External(_, name, _) => Some(name),
            Self::After(_) => None,
        }
    }

    fn external_event(&self) -> Option<&'a E> {
        match self {
            Self::Eventless(_, event, _) => *event,
            Self::External(_, _, event) => Some(*event),
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

pub(crate) struct HeadlessDomain<E>(PhantomData<fn() -> E>);

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
    now: Duration,
    timers: Vec<(Duration, TimerToken)>,
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

    /// Links and initializes a definition while delivering activity starts.
    pub fn new_with_activity_sink<S: StateTreeActivitySink<E> + ?Sized>(
        definition: &StateTreeDefinition,
        microstep_limit: NonZeroUsize,
        sink: &mut S,
    ) -> Result<Self, StateTreeLinkError> {
        let linked = Arc::new(LinkedDefinition::<HeadlessDomain<E>>::new(
            Arc::clone(&definition.compiled),
            None,
            None,
        )?);
        let mut instance = StateTreeInstance::new(Handle::default());
        instance.initialize_traced(
            linked,
            microstep_limit.get(),
            None,
            &mut RejectEffects,
            &mut HeadlessActivitySink { sink },
            &mut NoStateTreeTrace,
        );
        let mut execution = Self {
            instance,
            now: Duration::ZERO,
            timers: Vec::new(),
        };
        execution.apply_timer_updates();
        Ok(execution)
    }

    /// Links and initializes while delivering both effects and activity lifecycle changes.
    pub fn new_with_sinks<ES, AS>(
        definition: &StateTreeDefinition,
        microstep_limit: NonZeroUsize,
        effects: &mut ES,
        activities: &mut AS,
    ) -> Result<Self, StateTreeLinkError>
    where
        ES: StateTreeEffectSink<E> + ?Sized,
        AS: StateTreeActivitySink<E> + ?Sized,
    {
        Self::new_traced_with_sinks(
            definition,
            microstep_limit,
            effects,
            activities,
            &mut NoStateTreeTrace,
        )
    }

    /// Links and initializes with both output sinks and semantic tracing.
    pub fn new_traced_with_sinks<ES, AS, T>(
        definition: &StateTreeDefinition,
        microstep_limit: NonZeroUsize,
        effects: &mut ES,
        activities: &mut AS,
        trace: &mut T,
    ) -> Result<Self, StateTreeLinkError>
    where
        ES: StateTreeEffectSink<E> + ?Sized,
        AS: StateTreeActivitySink<E> + ?Sized,
        T: StateTreeTrace + ?Sized,
    {
        let linked = Arc::new(LinkedDefinition::<HeadlessDomain<E>>::new(
            Arc::clone(&definition.compiled),
            None,
            None,
        )?);
        let mut instance = StateTreeInstance::new(Handle::default());
        instance.initialize_traced(
            linked,
            microstep_limit.get(),
            None,
            &mut HeadlessEffectSink { sink: effects },
            &mut HeadlessActivitySink { sink: activities },
            trace,
        );
        let mut execution = Self {
            instance,
            now: Duration::ZERO,
            timers: Vec::new(),
        };
        execution.apply_timer_updates();
        Ok(execution)
    }

    /// Links and initializes while delivering activities and semantic traces.
    pub fn new_traced_with_activity_sink<
        S: StateTreeActivitySink<E> + ?Sized,
        T: StateTreeTrace + ?Sized,
    >(
        definition: &StateTreeDefinition,
        microstep_limit: NonZeroUsize,
        sink: &mut S,
        trace: &mut T,
    ) -> Result<Self, StateTreeLinkError> {
        let linked = Arc::new(LinkedDefinition::<HeadlessDomain<E>>::new(
            Arc::clone(&definition.compiled),
            None,
            None,
        )?);
        let mut instance = StateTreeInstance::new(Handle::default());
        instance.initialize_traced(
            linked,
            microstep_limit.get(),
            None,
            &mut RejectEffects,
            &mut HeadlessActivitySink { sink },
            trace,
        );
        let mut execution = Self {
            instance,
            now: Duration::ZERO,
            timers: Vec::new(),
        };
        execution.apply_timer_updates();
        Ok(execution)
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
            None,
        )?);
        let mut instance = StateTreeInstance::new(Handle::default());
        instance.initialize_traced(
            linked,
            microstep_limit.get(),
            None,
            &mut RejectEffects,
            &mut RejectActivities,
            trace,
        );
        let mut execution = Self {
            instance,
            now: Duration::ZERO,
            timers: Vec::new(),
        };
        execution.apply_timer_updates();
        Ok(execution)
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
            None,
        )?);
        let mut instance = StateTreeInstance::new(Handle::default());
        instance.initialize_traced(
            linked,
            microstep_limit.get(),
            None,
            &mut HeadlessEffectSink { sink },
            &mut RejectActivities,
            trace,
        );
        let mut execution = Self {
            instance,
            now: Duration::ZERO,
            timers: Vec::new(),
        };
        execution.apply_timer_updates();
        Ok(execution)
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
        let processed = self.instance.process_event_traced(
            event,
            microstep_limit.get(),
            None,
            &mut HeadlessEffectSink { sink },
            &mut RejectActivities,
            &mut NoStateTreeTrace,
        );
        self.apply_timer_updates();
        processed
    }

    /// Processes one external event while delivering activity lifecycle changes.
    pub fn process_event_with_activity_sink<S: StateTreeActivitySink<E> + ?Sized>(
        &mut self,
        event: &E,
        microstep_limit: NonZeroUsize,
        sink: &mut S,
    ) -> bool {
        let processed = self.instance.process_event_traced(
            event,
            microstep_limit.get(),
            None,
            &mut RejectEffects,
            &mut HeadlessActivitySink { sink },
            &mut NoStateTreeTrace,
        );
        self.apply_timer_updates();
        processed
    }

    /// Processes one external event while delivering both effects and activities.
    pub fn process_event_with_sinks<ES, AS>(
        &mut self,
        event: &E,
        microstep_limit: NonZeroUsize,
        effects: &mut ES,
        activities: &mut AS,
    ) -> bool
    where
        ES: StateTreeEffectSink<E> + ?Sized,
        AS: StateTreeActivitySink<E> + ?Sized,
    {
        self.process_event_traced_with_sinks(
            event,
            microstep_limit,
            effects,
            activities,
            &mut NoStateTreeTrace,
        )
    }

    /// Processes one external event with both output sinks and semantic tracing.
    pub fn process_event_traced_with_sinks<ES, AS, T>(
        &mut self,
        event: &E,
        microstep_limit: NonZeroUsize,
        effects: &mut ES,
        activities: &mut AS,
        trace: &mut T,
    ) -> bool
    where
        ES: StateTreeEffectSink<E> + ?Sized,
        AS: StateTreeActivitySink<E> + ?Sized,
        T: StateTreeTrace + ?Sized,
    {
        let processed = self.instance.process_event_traced(
            event,
            microstep_limit.get(),
            None,
            &mut HeadlessEffectSink { sink: effects },
            &mut HeadlessActivitySink { sink: activities },
            trace,
        );
        self.apply_timer_updates();
        processed
    }

    /// Completes one current activity run with portable Luau event data.
    pub fn complete_activity<S: StateTreeActivitySink<E> + ?Sized>(
        &mut self,
        token: StateTreeHeadlessActivityToken,
        data: Option<LuauValue>,
        microstep_limit: NonZeroUsize,
        sink: &mut S,
    ) -> bool {
        self.process_activity(
            token,
            crate::activity::ActivityOutcomeKind::Complete,
            data,
            microstep_limit,
            sink,
        )
    }

    /// Fails one current activity run with portable Luau event data.
    pub fn fail_activity<S: StateTreeActivitySink<E> + ?Sized>(
        &mut self,
        token: StateTreeHeadlessActivityToken,
        data: Option<LuauValue>,
        microstep_limit: NonZeroUsize,
        sink: &mut S,
    ) -> bool {
        self.process_activity(
            token,
            crate::activity::ActivityOutcomeKind::Fail,
            data,
            microstep_limit,
            sink,
        )
    }

    /// Completes an activity while delivering both effects and activity changes.
    pub fn complete_activity_with_sinks<ES, AS>(
        &mut self,
        token: StateTreeHeadlessActivityToken,
        data: Option<LuauValue>,
        microstep_limit: NonZeroUsize,
        effects: &mut ES,
        activities: &mut AS,
    ) -> bool
    where
        ES: StateTreeEffectSink<E> + ?Sized,
        AS: StateTreeActivitySink<E> + ?Sized,
    {
        self.process_activity_with_sinks(
            (token, crate::activity::ActivityOutcomeKind::Complete, data),
            microstep_limit,
            &mut HeadlessEffectSink { sink: effects },
            &mut HeadlessActivitySink { sink: activities },
            &mut NoStateTreeTrace,
        )
    }

    /// Fails an activity while delivering both effects and activity changes.
    pub fn fail_activity_with_sinks<ES, AS>(
        &mut self,
        token: StateTreeHeadlessActivityToken,
        data: Option<LuauValue>,
        microstep_limit: NonZeroUsize,
        effects: &mut ES,
        activities: &mut AS,
    ) -> bool
    where
        ES: StateTreeEffectSink<E> + ?Sized,
        AS: StateTreeActivitySink<E> + ?Sized,
    {
        self.process_activity_with_sinks(
            (token, crate::activity::ActivityOutcomeKind::Fail, data),
            microstep_limit,
            &mut HeadlessEffectSink { sink: effects },
            &mut HeadlessActivitySink { sink: activities },
            &mut NoStateTreeTrace,
        )
    }

    /// Completes one activity while writing its semantic macrostep trace.
    pub fn complete_activity_traced<
        S: StateTreeActivitySink<E> + ?Sized,
        T: StateTreeTrace + ?Sized,
    >(
        &mut self,
        token: StateTreeHeadlessActivityToken,
        data: Option<LuauValue>,
        microstep_limit: NonZeroUsize,
        sink: &mut S,
        trace: &mut T,
    ) -> bool {
        self.process_activity_with_sinks(
            (token, crate::activity::ActivityOutcomeKind::Complete, data),
            microstep_limit,
            &mut RejectEffects,
            &mut HeadlessActivitySink { sink },
            trace,
        )
    }

    /// Fails one activity while writing its semantic macrostep trace.
    pub fn fail_activity_traced<
        S: StateTreeActivitySink<E> + ?Sized,
        T: StateTreeTrace + ?Sized,
    >(
        &mut self,
        token: StateTreeHeadlessActivityToken,
        data: Option<LuauValue>,
        microstep_limit: NonZeroUsize,
        sink: &mut S,
        trace: &mut T,
    ) -> bool {
        self.process_activity_with_sinks(
            (token, crate::activity::ActivityOutcomeKind::Fail, data),
            microstep_limit,
            &mut RejectEffects,
            &mut HeadlessActivitySink { sink },
            trace,
        )
    }

    /// Completes an activity with both output sinks and semantic tracing.
    pub fn complete_activity_traced_with_sinks<ES, AS, T>(
        &mut self,
        token: StateTreeHeadlessActivityToken,
        data: Option<LuauValue>,
        microstep_limit: NonZeroUsize,
        effects: &mut ES,
        activities: &mut AS,
        trace: &mut T,
    ) -> bool
    where
        ES: StateTreeEffectSink<E> + ?Sized,
        AS: StateTreeActivitySink<E> + ?Sized,
        T: StateTreeTrace + ?Sized,
    {
        self.process_activity_with_sinks(
            (token, crate::activity::ActivityOutcomeKind::Complete, data),
            microstep_limit,
            &mut HeadlessEffectSink { sink: effects },
            &mut HeadlessActivitySink { sink: activities },
            trace,
        )
    }

    /// Fails an activity with both output sinks and semantic tracing.
    pub fn fail_activity_traced_with_sinks<ES, AS, T>(
        &mut self,
        token: StateTreeHeadlessActivityToken,
        data: Option<LuauValue>,
        microstep_limit: NonZeroUsize,
        effects: &mut ES,
        activities: &mut AS,
        trace: &mut T,
    ) -> bool
    where
        ES: StateTreeEffectSink<E> + ?Sized,
        AS: StateTreeActivitySink<E> + ?Sized,
        T: StateTreeTrace + ?Sized,
    {
        self.process_activity_with_sinks(
            (token, crate::activity::ActivityOutcomeKind::Fail, data),
            microstep_limit,
            &mut HeadlessEffectSink { sink: effects },
            &mut HeadlessActivitySink { sink: activities },
            trace,
        )
    }

    fn process_activity<S: StateTreeActivitySink<E> + ?Sized>(
        &mut self,
        token: StateTreeHeadlessActivityToken,
        kind: crate::activity::ActivityOutcomeKind,
        data: Option<LuauValue>,
        microstep_limit: NonZeroUsize,
        sink: &mut S,
    ) -> bool {
        self.process_activity_with_sinks(
            (token, kind, data),
            microstep_limit,
            &mut RejectEffects,
            &mut HeadlessActivitySink { sink },
            &mut NoStateTreeTrace,
        )
    }

    fn process_activity_with_sinks<ES, AS, T>(
        &mut self,
        outcome: (
            StateTreeHeadlessActivityToken,
            crate::activity::ActivityOutcomeKind,
            Option<LuauValue>,
        ),
        microstep_limit: NonZeroUsize,
        effects: &mut ES,
        activities: &mut AS,
        trace: &mut T,
    ) -> bool
    where
        ES: RuntimeEffectSink<HeadlessDomain<E>> + ?Sized,
        AS: RuntimeActivitySink<HeadlessDomain<E>> + ?Sized,
        T: StateTreeTrace + ?Sized,
    {
        let (token, kind, data) = outcome;
        let outcome = crate::activity::ActivityOutcome {
            token: ErasedActivityToken {
                entity: Entity::PLACEHOLDER,
                runtime_id: token.runtime_id,
                activity: token.activity,
                generation: token.generation,
            },
            kind,
            data: Ok(data),
        };
        let processed = self.instance.process_activity_outcome_traced(
            outcome,
            microstep_limit.get(),
            None,
            effects,
            activities,
            trace,
        );
        self.apply_timer_updates();
        processed
    }

    /// Processes one external event while writing its complete semantic trace.
    pub fn process_event_traced<T: StateTreeTrace + ?Sized>(
        &mut self,
        event: &E,
        microstep_limit: NonZeroUsize,
        trace: &mut T,
    ) -> bool {
        let processed = self.instance.process_event_traced(
            event,
            microstep_limit.get(),
            None,
            &mut RejectEffects,
            &mut RejectActivities,
            trace,
        );
        self.apply_timer_updates();
        processed
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
        let processed = self.instance.process_event_traced(
            event,
            microstep_limit.get(),
            None,
            &mut HeadlessEffectSink { sink },
            &mut RejectActivities,
            trace,
        );
        self.apply_timer_updates();
        processed
    }

    /// Advances the headless clock and processes every newly due timeout in
    /// chronological deadline order.
    pub fn advance_time(&mut self, delta: Duration, microstep_limit: NonZeroUsize) -> bool {
        self.advance_time_traced(delta, microstep_limit, &mut NoStateTreeTrace)
    }

    /// Advances time while delivering effects to `sink`.
    pub fn advance_time_with_sink<S: StateTreeEffectSink<E> + ?Sized>(
        &mut self,
        delta: Duration,
        microstep_limit: NonZeroUsize,
        sink: &mut S,
    ) -> bool {
        self.advance_time_traced_with_sink(delta, microstep_limit, sink, &mut NoStateTreeTrace)
    }

    /// Advances time while delivering activity lifecycle changes.
    pub fn advance_time_with_activity_sink<S: StateTreeActivitySink<E> + ?Sized>(
        &mut self,
        delta: Duration,
        microstep_limit: NonZeroUsize,
        sink: &mut S,
    ) -> bool {
        self.advance_time_inner(
            delta,
            microstep_limit,
            &mut RejectEffects,
            &mut HeadlessActivitySink { sink },
            &mut NoStateTreeTrace,
        )
    }

    /// Advances time while delivering both effects and activity lifecycle changes.
    pub fn advance_time_with_sinks<ES, AS>(
        &mut self,
        delta: Duration,
        microstep_limit: NonZeroUsize,
        effects: &mut ES,
        activities: &mut AS,
    ) -> bool
    where
        ES: StateTreeEffectSink<E> + ?Sized,
        AS: StateTreeActivitySink<E> + ?Sized,
    {
        self.advance_time_traced_with_sinks(
            delta,
            microstep_limit,
            effects,
            activities,
            &mut NoStateTreeTrace,
        )
    }

    /// Advances time with both output sinks and semantic tracing.
    pub fn advance_time_traced_with_sinks<ES, AS, T>(
        &mut self,
        delta: Duration,
        microstep_limit: NonZeroUsize,
        effects: &mut ES,
        activities: &mut AS,
        trace: &mut T,
    ) -> bool
    where
        ES: StateTreeEffectSink<E> + ?Sized,
        AS: StateTreeActivitySink<E> + ?Sized,
        T: StateTreeTrace + ?Sized,
    {
        self.advance_time_inner(
            delta,
            microstep_limit,
            &mut HeadlessEffectSink { sink: effects },
            &mut HeadlessActivitySink { sink: activities },
            trace,
        )
    }

    /// Advances time while writing complete semantic traces for due timeouts.
    pub fn advance_time_traced<T: StateTreeTrace + ?Sized>(
        &mut self,
        delta: Duration,
        microstep_limit: NonZeroUsize,
        trace: &mut T,
    ) -> bool {
        self.advance_time_inner(
            delta,
            microstep_limit,
            &mut RejectEffects,
            &mut RejectActivities,
            trace,
        )
    }

    /// Advances time while delivering effects and writing semantic traces.
    pub fn advance_time_traced_with_sink<
        S: StateTreeEffectSink<E> + ?Sized,
        T: StateTreeTrace + ?Sized,
    >(
        &mut self,
        delta: Duration,
        microstep_limit: NonZeroUsize,
        sink: &mut S,
        trace: &mut T,
    ) -> bool {
        self.advance_time_inner(
            delta,
            microstep_limit,
            &mut HeadlessEffectSink { sink },
            &mut RejectActivities,
            trace,
        )
    }

    fn advance_time_inner<
        S: RuntimeEffectSink<HeadlessDomain<E>> + ?Sized,
        A: RuntimeActivitySink<HeadlessDomain<E>> + ?Sized,
        T: StateTreeTrace + ?Sized,
    >(
        &mut self,
        delta: Duration,
        microstep_limit: NonZeroUsize,
        effects: &mut S,
        activities: &mut A,
        trace: &mut T,
    ) -> bool {
        self.now = self.now.saturating_add(delta);
        self.instance.set_time(self.now);
        let mut transitioned = false;
        let mut due = Vec::new();
        self.timers.retain(|(deadline, token)| {
            if *deadline <= self.now {
                due.push((*deadline, *token));
                false
            } else {
                true
            }
        });
        due.sort_unstable();
        let mut start = 0;
        while start < due.len() {
            let deadline = due[start].0;
            let mut end = start + 1;
            while end < due.len() && due[end].0 == deadline {
                end += 1;
            }
            let tokens: Vec<_> = due[start..end].iter().map(|(_, token)| *token).collect();
            transitioned |= self.instance.process_timers_traced(
                &tokens,
                microstep_limit.get(),
                None,
                effects,
                activities,
                trace,
            );
            self.apply_timer_updates();
            start = end;
        }
        transitioned
    }

    fn apply_timer_updates(&mut self) {
        for update in self.instance.timer_updates.drain(..) {
            match update {
                TimerUpdate::Arm { token, deadline } => self.timers.push((deadline, token)),
                TimerUpdate::Cancel(token) => {
                    self.timers.retain(|(_, candidate)| *candidate != token);
                }
            }
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
#[component(on_remove = release_event_inbox::<D>)]
#[require(PendingStateTree<D>, StateTreeEventInbox<D>)]
pub struct StateTreeInstance<D: StateTreeDomain> {
    runtime_id: u64,
    definition: Handle<StateTreeDefinition>,
    linked: Option<Arc<LinkedDefinition<D>>>,
    active: Vec<StateId>,
    parallel: Option<Box<ParallelRuntime>>,
    internal_events: VecDeque<EventId>,
    now: Duration,
    timers: Vec<TimerSlot>,
    activities: Option<Box<[ActivitySlot]>>,
    timer_updates: Vec<TimerUpdate>,
    data: Vec<LuauValue>,
    status: InstanceStatus,
    marker: PhantomData<fn() -> D>,
}

#[derive(Clone, Copy, Debug, Default)]
struct ActivitySlot {
    generation: u64,
    active: bool,
}

fn release_event_inbox<D: StateTreeDomain>(mut world: DeferredWorld, context: HookContext) {
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
            now: Duration::ZERO,
            timers: Vec::new(),
            activities: None,
            timer_updates: Vec::new(),
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
            .filter_map(|(index, slot)| {
                let id = ActivityId(index as u32);
                linked.activity(id).cloned().map(|handler| {
                    (
                        handler,
                        ErasedActivityToken {
                            entity,
                            runtime_id: self.runtime_id,
                            activity: id,
                            generation: slot.generation,
                        },
                    )
                })
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
        entity: Option<Entity>,
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

    fn initialize_traced<
        S: RuntimeEffectSink<D> + ?Sized,
        A: RuntimeActivitySink<D> + ?Sized,
        T: StateTreeTrace + ?Sized,
    >(
        &mut self,
        linked: Arc<LinkedDefinition<D>>,
        microstep_limit: usize,
        entity: Option<Entity>,
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
        self.timers.clear();
        self.timers
            .resize(compiled.timers.len(), TimerSlot::default());
        self.activities = (!compiled.activities.is_empty())
            .then(|| vec![ActivitySlot::default(); compiled.activities.len()].into_boxed_slice());
        self.timer_updates.clear();
        self.data.clear();
        self.status = InstanceStatus::Running;
        if let Err(fault) = self.initialize_datamodel(compiled, trace) {
            self.set_fault(fault, entity, activities, trace);
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
        event: &D::Event,
        microstep_limit: usize,
        entity: Option<Entity>,
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

    fn process_event_traced<
        S: RuntimeEffectSink<D> + ?Sized,
        A: RuntimeActivitySink<D> + ?Sized,
        T: StateTreeTrace + ?Sized,
    >(
        &mut self,
        event: &D::Event,
        microstep_limit: usize,
        entity: Option<Entity>,
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
        entity: Option<Entity>,
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

    fn process_timers_traced<
        S: RuntimeEffectSink<D> + ?Sized,
        A: RuntimeActivitySink<D> + ?Sized,
        T: StateTreeTrace + ?Sized,
    >(
        &mut self,
        tokens: &[TimerToken],
        microstep_limit: usize,
        entity: Option<Entity>,
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
        entity: Option<Entity>,
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

    fn process_activity_outcome_traced<
        S: RuntimeEffectSink<D> + ?Sized,
        A: RuntimeActivitySink<D> + ?Sized,
        T: StateTreeTrace + ?Sized,
    >(
        &mut self,
        outcome: crate::activity::ActivityOutcome,
        microstep_limit: usize,
        entity: Option<Entity>,
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
        if !self.active.contains(&definition.owner) {
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
        let selected = self.select_step(Trigger::Internal(event, data.as_ref()));
        match selected {
            Ok(Some(selected)) => {
                let changed = self.selected_step_changes_configuration(selected);
                self.trace_selected_step(1, selected, trace);
                let context = ActionContext {
                    event_name: compiled.event_name(event),
                    external_event: None,
                    event_data: data.as_ref(),
                    entity,
                };
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
            Ok(None) => {}
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
        context: ActionContext<'_, D::Event>,
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
        trigger: Trigger<'_, D::Event>,
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
                    (TransitionTrigger::Event(event), Trigger::External(events, _, _)) => {
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
                    let matches = match (transition.trigger, trigger) {
                        (TransitionTrigger::Eventless, Trigger::Eventless(_, _, _)) => true,
                        (TransitionTrigger::Event(candidate), Trigger::Internal(emitted, _)) => {
                            compiled.internal_event_matches(candidate, *emitted)
                        }
                        (TransitionTrigger::Event(event), Trigger::External(events, _, _)) => {
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

    fn take_selected_step<
        S: RuntimeEffectSink<D> + ?Sized,
        A: RuntimeActivitySink<D> + ?Sized,
        T: StateTreeTrace + ?Sized,
    >(
        &mut self,
        selected: SelectedStep,
        context: ActionContext<'_, D::Event>,
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
        context: ActionContext<'_, D::Event>,
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
            self.cancel_state_timers(&compiled, *state);
            self.cancel_state_activities(*state, context.entity, activities, trace);
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
        context: ActionContext<'_, D::Event>,
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
            self.enter_state(&compiled, *entry, context, effects, activities, trace)?;
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

    fn enter_state<
        S: RuntimeEffectSink<D> + ?Sized,
        A: RuntimeActivitySink<D> + ?Sized,
        T: StateTreeTrace + ?Sized,
    >(
        &mut self,
        compiled: &CompiledDefinition,
        entry: Entry,
        context: ActionContext<'_, D::Event>,
        effects: &mut S,
        activities: &mut A,
        trace: &mut T,
    ) -> Result<(), RuntimeFault> {
        self.active.push(entry.state);
        let state = compiled.state(entry.state);
        trace.record(StateTreeTraceRecord::StateEntered { state: &state.name });
        self.arm_state_timers(compiled, entry.state);
        self.execute_actions(compiled, &state.entry_actions, context, effects, trace)?;
        self.start_state_activities(compiled, entry.state, context, activities, trace)?;
        if entry.take_initial {
            self.execute_actions(compiled, &state.initial_actions, context, effects, trace)?;
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
        context: ActionContext<'_, D::Event>,
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
                entity: context.entity.unwrap_or(Entity::PLACEHOLDER),
                runtime_id: self.runtime_id,
                activity: id,
                generation,
            };
            let handler = self.linked.as_ref().and_then(|linked| linked.activity(id));
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
                id: &definition.id,
                name: &definition.name,
                handler,
                context: activity_context,
                arguments: StateTreeActivityArgs::new(values),
                token,
                span_start: definition.span_start as usize,
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
        entity: Option<Entity>,
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
                entity: entity.unwrap_or(Entity::PLACEHOLDER),
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
        entity: Option<Entity>,
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
        entity: Option<Entity>,
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
        Ok(())
    }

    fn evaluate_condition(
        &self,
        compiled: &CompiledDefinition,
        expression: ExpressionId,
        event_name: Option<&str>,
        external_event: Option<&D::Event>,
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
        external_event: Option<&D::Event>,
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

    fn update_completion<A: RuntimeActivitySink<D> + ?Sized, T: StateTreeTrace + ?Sized>(
        &mut self,
        entity: Option<Entity>,
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
        entity: Option<Entity>,
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
            self.active.contains(child) && compiled.state(*child).kind == StateKind::Final
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
    use std::time::Duration;

    use bevy_asset::Handle;

    use super::*;
    use crate::{
        StateTreeAction, StateTreeBuilder, StateTreeData, StateTreeEventLog, StateTreeTransition,
    };

    struct TestDomain;

    enum TestEvents {
        Advance,
        Finish,
    }

    impl crate::StateTreeEvents for TestEvents {
        const AUTHORED_NAMES: &'static [&'static str] = &["advance", "finish"];

        fn event_index(&self) -> usize {
            match self {
                Self::Advance => 0,
                Self::Finish => 1,
            }
        }
    }

    impl crate::StateTreeDomain for TestDomain {
        type Event = TestEvents;
    }

    fn instance(definition: &StateTreeDefinition, limit: usize) -> StateTreeInstance<TestDomain> {
        let mut instance = StateTreeInstance::new(Handle::default());
        instance.initialize(
            Arc::new(
                LinkedDefinition::<TestDomain>::new(Arc::clone(&definition.compiled), None, None)
                    .unwrap(),
            ),
            limit,
            None,
            &mut RejectEffects,
            &mut RejectActivities,
        );
        instance
    }

    fn process_event(
        instance: &mut StateTreeInstance<TestDomain>,
        event: &TestEvents,
        limit: usize,
    ) -> bool {
        instance.process_event(
            event,
            limit,
            None,
            &mut RejectEffects,
            &mut RejectActivities,
        )
    }

    fn execution(definition: &StateTreeDefinition) -> StateTreeExecution<TestEvents> {
        StateTreeExecution::new(definition, NonZeroUsize::new(64).unwrap()).unwrap()
    }

    #[test]
    fn initialized_definitions_without_activities_have_no_activity_slot_storage() {
        let mut builder = StateTreeBuilder::new("no-activities");
        let root = builder.compound_root("root").unwrap();
        let idle = builder.atomic("idle", root).unwrap();
        builder.initial(root, idle).unwrap();
        let definition = builder.build().unwrap();

        let instance = instance(&definition, 8);

        assert!(instance.activities.is_none());
    }

    fn advance(execution: &mut StateTreeExecution<TestEvents>, duration: Duration) -> bool {
        execution.advance_time(duration, NonZeroUsize::new(64).unwrap())
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
        builder
            .add_transition(waiting, StateTreeTransition::on("combat").target(done))
            .unwrap();
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
    fn headless_after_transition_fires_only_at_its_deadline() {
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
        let limit = NonZeroUsize::new(64).unwrap();

        advance(&mut execution, Duration::from_millis(500));
        execution.process_event(&TestEvents::Advance, limit);
        assert!(!advance(&mut execution, Duration::from_secs(1)));
        execution.process_event(&TestEvents::Finish, limit);
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
        let mut trace = StateTreeEventLog::new();
        let mut execution = StateTreeExecution::<TestEvents>::new_traced(
            &definition,
            NonZeroUsize::new(64).unwrap(),
            &mut trace,
        )
        .unwrap();

        assert!(execution.advance_time_traced(
            Duration::from_secs(1),
            NonZeroUsize::new(64).unwrap(),
            &mut trace,
        ));
        let active = execution.active_state_names().collect::<Vec<_>>();
        assert!(active.contains(&"left-elapsed"));
        assert!(active.contains(&"right-elapsed"));
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
        let mut fault =
            StateTreeExecution::<TestEvents>::new(&definition, NonZeroUsize::new(2).unwrap())
                .unwrap();

        assert!(fault.advance_time(Duration::from_secs(1), NonZeroUsize::new(2).unwrap()));
        assert_eq!(
            fault.status(),
            &InstanceStatus::Faulted(RuntimeFault::MicrostepLimitExceeded { limit: 2 })
        );
        assert!(!fault.advance_time(Duration::from_secs(1), NonZeroUsize::new(2).unwrap()));
    }
}
