use super::*;
use crate::persistence::{Error, ResolvedRun, Value, invalid};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct RuntimeSnapshot {
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

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SavedTimer {
    generation: u64,
    armed: bool,
    remaining: Option<Duration>,
}

pub(crate) fn fingerprint(compiled: &CompiledDefinition) -> Result<[u8; 32], Error> {
    Ok(*blake3::hash(&serde_json::to_vec(compiled)?).as_bytes())
}

impl<D: StateTreeDomain> StateTreeInstance<D> {
    pub(crate) fn active_activity_tokens_for_persistence(
        &self,
        entity: Entity,
    ) -> Vec<(ErasedActivityToken, &'static str)> {
        self.active_activity_tokens(entity)
            .into_iter()
            .map(|(handler, token)| (token, handler.type_name()))
            .collect()
    }

    pub(crate) fn persistent_runs(&self, entity: Entity) -> Vec<ResolvedRun> {
        self.active_activity_tokens(entity)
            .into_iter()
            .map(|(handler, token)| ResolvedRun {
                token,
                domain: D::AUTHORED_NAME.into(),
                activity_type: handler.type_name().into(),
            })
            .collect()
    }

    pub(crate) fn persistent_runtime(
        &self,
        remaining: impl Fn(TimerToken) -> Option<Duration>,
    ) -> RuntimeSnapshot {
        let runtime = &self.runtime;
        RuntimeSnapshot {
            active: runtime
                .active
                .iter()
                .map(|state| state.index() as u32)
                .collect(),
            completed: runtime.parallel.as_ref().map_or_else(Vec::new, |parallel| {
                parallel
                    .completed
                    .iter()
                    .map(|state| state.index() as u32)
                    .collect()
            }),
            internal_events: runtime
                .internal_events
                .iter()
                .filter_map(|event| {
                    self.linked
                        .as_ref()?
                        .compiled
                        .event_name(*event)
                        .map(String::from)
                })
                .collect(),
            timers: runtime
                .timers
                .iter()
                .enumerate()
                .map(|(index, timer)| SavedTimer {
                    generation: timer.generation,
                    armed: timer.armed,
                    remaining: remaining(TimerToken {
                        runtime_id: runtime.runtime_id,
                        timer: TimerId(index as u32),
                        generation: timer.generation,
                    }),
                })
                .collect(),
            activities: runtime
                .activities
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|slot| (slot.generation, slot.active))
                .collect(),
            history: runtime.history.as_ref().map_or_else(Vec::new, |history| {
                history
                    .values
                    .iter()
                    .map(|state| state.map(|state| state.index() as u32))
                    .collect()
            }),
            initial_data: runtime
                .initial_data
                .values
                .iter()
                .map(|(key, value)| (key.clone(), Value::from(value)))
                .collect(),
            data: runtime.data.iter().map(Value::from).collect(),
            status: runtime.status.clone(),
        }
    }

    pub(crate) fn from_persistent_runtime(
        handle: Handle<StateTreeDefinition>,
        linked: Option<Arc<LinkedDefinition<D>>>,
        saved: &RuntimeSnapshot,
        now: Duration,
    ) -> Result<Self, Error> {
        let mut instance = Self::new_with_initial_data(
            handle,
            StateTreeDataOverrides::try_new(
                saved
                    .initial_data
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone().into())),
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
        runtime.data = saved.data.iter().cloned().map(Into::into).collect();
        runtime.status = saved.status.clone();
        instance.linked = Some(linked);
        Ok(instance)
    }

    pub(crate) fn persistent_definition(&self) -> Option<&Arc<LinkedDefinition<D>>> {
        self.linked.as_ref()
    }
}
