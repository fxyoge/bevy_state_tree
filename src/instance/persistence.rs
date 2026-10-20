use super::*;
use crate::persistence::{Error, ResolvedRun, Value, invalid};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

/// Symbolic execution state. Keys refer to authored identities, never compiled slots.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuntimeSnapshot {
    pub active: BTreeMap<String, SavedState>,
    pub completed: Vec<String>,
    pub internal_events: Vec<String>,
    pub timers: BTreeMap<String, BTreeMap<String, SavedTimer>>,
    pub activities: BTreeMap<String, SavedActivity>,
    pub history: BTreeMap<String, SavedHistory>,
    pub initial_data: BTreeMap<Box<str>, Value>,
    pub data: BTreeMap<Box<str>, Value>,
    pub status: InstanceStatus,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedState {
    pub parent: Option<String>,
    pub kind: SavedStateKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SavedStateKind {
    Atomic,
    Compound,
    Parallel,
    Final,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedActivity {
    pub owner: String,
    pub handler: String,
    pub generation: u64,
    pub active: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedHistory {
    pub owner: String,
    pub child: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct DefinitionLayout {
    states: BTreeMap<Box<str>, StateId>,
    timers: Vec<(StateId, Box<str>)>,
}

struct ResolvedSnapshot {
    active: Vec<u32>,
    completed: Vec<u32>,
    internal_events: Vec<String>,
    timers: Vec<SavedTimer>,
    activities: Vec<(u64, bool)>,
    history: Vec<Option<u32>>,
    initial_data: Vec<(Box<str>, Value)>,
    data: Vec<Value>,
    status: InstanceStatus,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SavedTimer {
    pub generation: u64,
    pub armed: bool,
    pub remaining: Option<Duration>,
}

const ROOT: &str = "#root";

fn state_key(compiled: &CompiledDefinition, state: StateId) -> &str {
    if state == compiled.root {
        ROOT
    } else {
        &compiled.state(state).name
    }
}

fn state_kind(kind: StateKind) -> SavedStateKind {
    match kind {
        StateKind::Atomic => SavedStateKind::Atomic,
        StateKind::Final => SavedStateKind::Final,
        StateKind::Compound { .. } => SavedStateKind::Compound,
        StateKind::Parallel { .. } => SavedStateKind::Parallel,
    }
}

pub(crate) fn validate_definition(compiled: &CompiledDefinition) -> Result<(), Error> {
    definition_layout(compiled).map(|_| ())
}

fn definition_layout(compiled: &CompiledDefinition) -> Result<&DefinitionLayout, Error> {
    compiled
        .persistence_layout
        .get_or_init(|| build_layout(compiled))
        .as_ref()
        .map_err(|message| invalid(message.clone()))
}

fn build_layout(compiled: &CompiledDefinition) -> Result<DefinitionLayout, String> {
    if compiled.has_anonymous_names {
        return Err(format!(
            "definition `{}` requires explicit state and history ids for persistence",
            compiled.name
        ));
    }
    if compiled
        .states
        .iter()
        .enumerate()
        .any(|(index, state)| index != compiled.root.index() && state.name.as_ref() == ROOT)
    {
        return Err("state id `#root` is reserved for persistence".into());
    }
    if compiled.timer_names.iter().any(Option::is_none) {
        return Err(format!(
            "definition `{}` requires explicit timer ids for persistence",
            compiled.name
        ));
    }
    let mut states = BTreeMap::new();
    let mut timers = Vec::new();
    for (index, node) in compiled.states.iter().enumerate() {
        let state = compiled.persistent_state(index as u32).unwrap();
        states.insert(state_key(compiled, state).into(), state);
        for transition in &node.transitions {
            if let TransitionTrigger::After(timer) = transition.trigger {
                debug_assert_eq!(timers.len(), timer.index());
                timers.push((state, compiled.timer_names[timer.index()].clone().unwrap()));
            }
        }
    }
    Ok(DefinitionLayout { states, timers })
}

impl<D: StateTreeDomain> StateTreeInstance<D> {
    pub(crate) fn active_activity_tokens_for_persistence(
        &self,
        entity: Entity,
    ) -> Vec<ResolvedRun> {
        self.active_activity_tokens(entity)
            .into_iter()
            .map(|(handler, token)| ResolvedRun {
                activity: self.linked.as_ref().unwrap().compiled.activities[token.activity.index()]
                    .id
                    .to_string(),
                token,
                domain: D::AUTHORED_NAME.into(),
                activity_type: handler.type_id(),
            })
            .collect()
    }

    pub(crate) fn persistent_runs(&self, entity: Entity) -> Vec<ResolvedRun> {
        self.active_activity_tokens_for_persistence(entity)
    }

    pub(crate) fn persistent_runtime(
        &self,
        remaining: impl Fn(TimerToken) -> Option<Duration>,
    ) -> Result<RuntimeSnapshot, Error> {
        let runtime = &self.runtime;
        let initial_data = runtime
            .initial_data
            .values
            .iter()
            .map(|(k, v)| (k.clone(), Value::from(v)))
            .collect();
        let mut saved = RuntimeSnapshot {
            active: BTreeMap::new(),
            completed: Vec::new(),
            internal_events: Vec::new(),
            timers: BTreeMap::new(),
            activities: BTreeMap::new(),
            history: BTreeMap::new(),
            initial_data,
            data: BTreeMap::new(),
            status: runtime.status.clone(),
        };
        let Some(linked) = self.linked.as_ref() else {
            return Ok(saved);
        };
        let compiled = &linked.compiled;
        let layout = definition_layout(compiled)?;
        for state in &runtime.active {
            let node = compiled.state(*state);
            saved.active.insert(
                state_key(compiled, *state).into(),
                SavedState {
                    parent: node.parent.map(|parent| state_key(compiled, parent).into()),
                    kind: state_kind(node.kind),
                },
            );
        }
        if let Some(parallel) = &runtime.parallel {
            saved.completed = parallel
                .completed
                .iter()
                .map(|state| state_key(compiled, *state).into())
                .collect();
        }
        saved.internal_events = runtime
            .internal_events
            .iter()
            .map(|event| compiled.event_name(*event).unwrap().into())
            .collect();
        for (index, (owner, name)) in layout.timers.iter().enumerate() {
            let timer = TimerId(index as u32);
            let slot = &runtime.timers[index];
            if runtime.active.contains(owner) {
                saved
                    .timers
                    .entry(state_key(compiled, *owner).into())
                    .or_default()
                    .insert(
                        name.to_string(),
                        SavedTimer {
                            generation: slot.generation,
                            armed: slot.armed,
                            remaining: remaining(TimerToken {
                                runtime_id: runtime.runtime_id,
                                timer,
                                generation: slot.generation,
                            }),
                        },
                    );
            }
        }
        for (index, slot) in runtime
            .activities
            .as_deref()
            .unwrap_or_default()
            .iter()
            .enumerate()
        {
            let activity = &compiled.activities[index];
            if runtime.active.contains(&activity.owner) {
                saved.activities.insert(
                    activity.id.to_string(),
                    SavedActivity {
                        owner: state_key(compiled, activity.owner).into(),
                        handler: activity.name.to_string(),
                        generation: slot.generation,
                        active: slot.active,
                    },
                );
            }
        }
        for history in &compiled.histories {
            let child = runtime
                .history
                .as_ref()
                .and_then(|h| h.values[history.slot.index()]);
            if child.is_none() {
                continue;
            }
            saved.history.insert(
                history.name.to_string(),
                SavedHistory {
                    owner: state_key(compiled, history.owner).into(),
                    child: child.map(|state| state_key(compiled, state).into()),
                },
            );
        }
        if let Some(data) = &compiled.datamodel {
            saved.data = data
                .names
                .iter()
                .cloned()
                .zip(runtime.data.iter().map(Value::from))
                .collect();
        }
        Ok(saved)
    }

    pub(crate) fn from_persistent_runtime(
        handle: Handle<StateTreeDefinition>,
        linked: Option<Arc<LinkedDefinition<D>>>,
        saved: &RuntimeSnapshot,
        now: Duration,
    ) -> Result<Self, Error> {
        let mut resolved = ResolvedSnapshot {
            active: Vec::new(),
            completed: Vec::new(),
            internal_events: saved.internal_events.clone(),
            timers: Vec::new(),
            activities: Vec::new(),
            history: Vec::new(),
            initial_data: saved
                .initial_data
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            data: Vec::new(),
            status: saved.status.clone(),
        };
        if saved.status == InstanceStatus::Pending {
            if !saved.active.is_empty()
                || !saved.completed.is_empty()
                || !saved.timers.is_empty()
                || !saved.activities.is_empty()
                || !saved.history.is_empty()
                || !saved.data.is_empty()
            {
                return Err(invalid("pending tree contains initialized execution state"));
            }
            return Self::restore_resolved(handle, linked, resolved, now);
        }
        let definition = linked
            .as_ref()
            .ok_or_else(|| invalid("initialized tree has no linked definition"))?;
        let compiled = &definition.compiled;
        let layout = definition_layout(compiled)?;
        let names = &layout.states;
        let state = |name: &str| {
            names
                .get(name)
                .copied()
                .ok_or_else(|| invalid(format!("migration required: unknown saved state `{name}`")))
        };
        for (name, expected) in &saved.active {
            let id = state(name)?;
            let current = compiled.state(id);
            if expected.parent.as_deref() != current.parent.map(|id| state_key(compiled, id))
                || expected.kind != state_kind(current.kind)
            {
                return Err(invalid(format!(
                    "migration required: state `{name}` changed parent or kind"
                )));
            }
            resolved.active.push(id.index() as u32);
        }
        resolved
            .active
            .sort_by_key(|index| compiled.states[*index as usize].document_order);
        resolved.completed = saved
            .completed
            .iter()
            .map(|name| state(name).map(|s| s.index() as u32))
            .collect::<Result<_, _>>()?;
        let mut timers = saved.timers.clone();
        resolved
            .timers
            .resize_with(compiled.timers.len(), SavedTimer::default);
        for (index, (owner, timer_name)) in layout.timers.iter().enumerate() {
            let name = state_key(compiled, *owner);
            if let Some(value) = timers
                .get_mut(name)
                .and_then(|entries| entries.remove(timer_name.as_ref()))
            {
                resolved.timers[index] = value;
            } else if saved.active.contains_key(name) {
                return Err(invalid(format!(
                    "migration required: new timer `{name}/{timer_name}` in active state"
                )));
            }
        }
        if let Some((owner, entries)) = timers.iter().find(|(_, entries)| !entries.is_empty()) {
            return Err(invalid(format!(
                "migration required: removed timer `{owner}/{}`",
                entries.keys().next().unwrap()
            )));
        }
        let mut activities = saved.activities.clone();
        for activity in &compiled.activities {
            let owner = state_key(compiled, activity.owner);
            if let Some(value) = activities.remove(activity.id.as_ref()) {
                if value.owner != owner || value.handler != activity.name.as_ref() {
                    return Err(invalid(format!(
                        "migration required: activity `{}` changed owner or handler",
                        activity.id
                    )));
                }
                resolved.activities.push((value.generation, value.active));
            } else if saved.active.contains_key(owner) {
                return Err(invalid(format!(
                    "migration required: new activity `{}` in active state",
                    activity.id
                )));
            } else {
                resolved.activities.push((0, false));
            }
        }
        if let Some(id) = activities.keys().next() {
            return Err(invalid(format!(
                "migration required: removed saved activity `{id}`"
            )));
        }
        let mut history = saved.history.clone();
        resolved.history.resize(compiled.history_slots.len(), None);
        let mut assigned = HashSet::new();
        for current in &compiled.histories {
            if let Some(value) = history.remove(current.name.as_ref()) {
                if value.owner != state_key(compiled, current.owner) {
                    return Err(invalid("migration required: history owner changed"));
                }
                let value = value
                    .child
                    .as_deref()
                    .map(state)
                    .transpose()?
                    .map(|s| s.index() as u32);
                let slot = current.slot.index();
                if !assigned.insert(slot) && resolved.history[slot] != value {
                    return Err(invalid("conflicting history memories for one owner"));
                }
                resolved.history[slot] = value;
            }
        }
        if let Some(id) = history.keys().next() {
            return Err(invalid(format!(
                "migration required: removed saved history `{id}`"
            )));
        }
        let mut values = saved.data.clone();
        let mut missing = Vec::new();
        if let Some(data) = &compiled.datamodel {
            for (index, name) in data.names.iter().enumerate() {
                resolved.data.push(values.remove(name).unwrap_or_else(|| {
                    missing.push(index);
                    Value::Nil
                }));
            }
        }
        if let Some(name) = values.keys().next() {
            return Err(invalid(format!(
                "migration required: removed saved datamodel variable `{name}`"
            )));
        }
        let mut instance = Self::restore_resolved(handle, linked.clone(), resolved, now)?;
        if let Some(data) = &compiled.datamodel {
            for index in missing {
                if let Some(initializer) = data.initializers[index] {
                    instance.runtime.data[index] = crate::luau::evaluate(
                        &data.expressions[initializer.index()],
                        &data.names,
                        &instance.runtime.data,
                        None,
                        None,
                    )
                    .map_err(|error| {
                        invalid(format!(
                            "initializing new saved variable `{}`: {error}",
                            data.names[index]
                        ))
                    })?;
                }
                if let Some(value) = saved.initial_data.get(&data.names[index]) {
                    instance.runtime.data[index] = value.clone().into();
                }
            }
        }
        Ok(instance)
    }

    fn restore_resolved(
        handle: Handle<StateTreeDefinition>,
        linked: Option<Arc<LinkedDefinition<D>>>,
        saved: ResolvedSnapshot,
        now: Duration,
    ) -> Result<Self, Error> {
        let mut instance = Self::new_with_initial_data(
            handle,
            StateTreeDataOverrides::try_new(
                saved.initial_data.into_iter().map(|(k, v)| (k, v.into())),
            )
            .map_err(|e| invalid(e.to_string()))?,
        );
        if saved.status == InstanceStatus::Pending {
            if !saved.active.is_empty()
                || !saved.activities.is_empty()
                || !saved.timers.is_empty()
                || !saved.history.is_empty()
                || !saved.data.is_empty()
                || !saved.completed.is_empty()
                || !saved.internal_events.is_empty()
            {
                return Err(invalid("pending tree contains initialized execution state"));
            }
            return Ok(instance);
        }
        let linked = linked.ok_or_else(|| invalid("initialized tree has no linked definition"))?;
        let compiled = &linked.compiled;
        let state = |index| {
            compiled
                .persistent_state(index)
                .ok_or_else(|| invalid("invalid state index"))
        };
        let active = saved
            .active
            .iter()
            .map(|index| state(*index))
            .collect::<Result<Vec<_>, _>>()?;
        let active_set: std::collections::HashSet<_> = active.iter().copied().collect();
        if active_set.len() != active.len()
            || !active.windows(2).all(|pair| {
                compiled.state(pair[0]).document_order < compiled.state(pair[1]).document_order
            })
        {
            return Err(invalid("active configuration is not canonical"));
        }
        for active in &active {
            if let Some(parent) = compiled.state(*active).parent
                && !active_set.contains(&parent)
            {
                return Err(invalid("active state has an inactive parent"));
            }
        }
        if matches!(
            saved.status,
            InstanceStatus::Running | InstanceStatus::Complete
        ) {
            if !active_set.contains(&compiled.root) {
                return Err(invalid("active root is missing"));
            }
            for active in &active {
                let children = compiled.children(*active);
                let count = children
                    .iter()
                    .filter(|child| active_set.contains(child))
                    .count();
                match compiled.state(*active).kind {
                    StateKind::Compound { .. } if count != 1 => {
                        return Err(invalid("invalid compound configuration"));
                    }
                    StateKind::Parallel { .. } if count != children.len() => {
                        return Err(invalid("incomplete parallel configuration"));
                    }
                    _ => {}
                }
            }
            let final_root = compiled.children(compiled.root).iter().any(|child| {
                active_set.contains(child) && compiled.state(*child).kind == StateKind::Final
            });
            if (saved.status == InstanceStatus::Complete) != final_root {
                return Err(invalid("completion status does not match configuration"));
            }
        }
        if saved.timers.len() != compiled.timers.len()
            || saved.activities.len() != compiled.activities.len()
            || saved.history.len() != compiled.history_slots.len()
            || saved.data.len()
                != compiled
                    .datamodel
                    .as_ref()
                    .map_or(0, |data| data.names.len())
        {
            return Err(invalid("runtime storage does not match definition"));
        }
        let runtime = &mut instance.runtime;
        runtime.now = now;
        runtime.active = active;
        runtime.active_states = compiled.has_parallel.then(|| {
            let mut values = vec![false; compiled.states.len()];
            for active in &runtime.active {
                values[active.index()] = true;
            }
            values.into_boxed_slice()
        });
        runtime.parallel = compiled.has_parallel.then(|| {
            Box::new(ParallelRuntime::new(
                compiled.states.len(),
                compiled.max_active,
            ))
        });
        if let Some(parallel) = runtime.parallel.as_mut() {
            for completed in &saved.completed {
                let state = state(*completed)?;
                if !active_set.contains(&state)
                    || parallel.completed_states[state.index()]
                    || !matches!(
                        compiled.state(state).kind,
                        StateKind::Compound { .. } | StateKind::Parallel { .. }
                    )
                {
                    return Err(invalid("invalid completion bookkeeping"));
                }
                parallel.completed_states[state.index()] = true;
                parallel.completed.push(state);
            }
        } else if !saved.completed.is_empty() {
            return Err(invalid("unexpected parallel completion bookkeeping"));
        }
        runtime.internal_events = saved
            .internal_events
            .iter()
            .map(|name| {
                compiled
                    .persistent_event(name)
                    .ok_or_else(|| invalid("unknown internal event"))
            })
            .collect::<Result<_, _>>()?;
        for (index, timer) in saved.timers.iter().enumerate() {
            if !timer.armed && timer.remaining.is_some() {
                return Err(invalid("inactive timer has a deadline"));
            }
            if timer.armed
                && (timer.generation == 0
                    || saved.status != InstanceStatus::Running
                    || !runtime.active.iter().any(|state| {
                        compiled.state(*state).transitions.iter().any(|transition| {
                            transition.trigger == TransitionTrigger::After(TimerId(index as u32))
                        })
                    }))
            {
                return Err(invalid("armed timer has no active owner"));
            }
            runtime.timers.push(TimerSlot {
                generation: timer.generation,
                armed: timer.armed,
            });
            if let Some(remaining) = timer.remaining {
                let deadline = now
                    .checked_add(remaining)
                    .ok_or_else(|| invalid("restored timer deadline overflows clock"))?;
                runtime.timer_updates.push(TimerUpdate::Arm {
                    token: TimerToken {
                        runtime_id: runtime.runtime_id,
                        timer: TimerId(index as u32),
                        generation: timer.generation,
                    },
                    deadline,
                });
            }
        }
        if !saved.activities.is_empty() {
            let mut slots = Vec::new();
            for (index, (generation, active)) in saved.activities.iter().enumerate() {
                if *active
                    && (*generation == 0
                        || saved.status != InstanceStatus::Running
                        || !active_set.contains(&compiled.activities[index].owner))
                {
                    return Err(invalid("live activity has no active owner"));
                }
                slots.push(ActivitySlot {
                    generation: *generation,
                    active: *active,
                });
            }
            runtime.activities = Some(slots.into_boxed_slice());
        }
        if !saved.history.is_empty() {
            let values = saved
                .history
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    let value = value.map(state).transpose()?;
                    if let Some(value) = value
                        && compiled.state(value).parent != Some(compiled.history_slots[index].owner)
                    {
                        return Err(invalid("history value is not an owner's direct child"));
                    }
                    Ok(value)
                })
                .collect::<Result<Vec<_>, Error>>()?;
            runtime.history = Some(Box::new(HistoryRuntime {
                values: values.into_boxed_slice(),
                capturing: vec![false; compiled.history_slots.len()].into_boxed_slice(),
                entries: Vec::with_capacity(compiled.max_active),
                alternate: Vec::with_capacity(compiled.max_active),
            }));
        }
        runtime.data = saved.data.into_iter().map(Into::into).collect();
        runtime.status = saved.status;
        instance.linked = Some(linked);
        Ok(instance)
    }

    pub(crate) fn persistent_definition(&self) -> Option<&Arc<LinkedDefinition<D>>> {
        self.linked.as_ref()
    }
}
