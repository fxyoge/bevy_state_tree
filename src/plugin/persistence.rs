use super::*;
use crate::{
    activity::{ActivityOutcome, ActivityOutcomeKind},
    instance::persistence::fingerprint,
    persistence::{
        Apply, DomainCodec, DomainSnapshot, Error, Registry, ResolvedRun, SavedEvent, SavedOutcome,
        TreeSnapshot, Value, entity_id, invalid, loaded_run, query_all, run_id,
    },
};
use bevy_ecs::world::World;

struct Domain<D>(PhantomData<fn() -> D>);

pub(super) fn register<D: StateTreeDomain>(app: &mut App) {
    app.init_resource::<Registry>();
    app.world_mut()
        .resource_mut::<Registry>()
        .domains
        .insert(D::AUTHORED_NAME.into(), Arc::new(Domain::<D>(PhantomData)));
}

fn save_event<D: StateTreeDomain>(
    event: &QueuedStateTreeEvent<D>,
    world: &World,
) -> Result<SavedEvent, Error> {
    let protocol = world.resource::<StateTreeAppEventRegistry<D>>().protocol();
    event.persistent_snapshot(protocol)
}

impl<D: StateTreeDomain> DomainCodec for Domain<D> {
    fn entities(&self, world: &mut World) -> Vec<Entity> {
        query_all::<Entity, With<StateTreeInstance<D>>>(world)
            .iter(world)
            .collect()
    }

    fn runs(&self, world: &mut World) -> Vec<(crate::activity::ErasedActivityToken, &'static str)> {
        query_all::<(Entity, &StateTreeInstance<D>), ()>(world)
            .iter(world)
            .flat_map(|(entity, instance)| instance.active_activity_tokens_for_persistence(entity))
            .collect()
    }

    fn capture(&self, world: &mut World) -> Result<DomainSnapshot, Error> {
        let now = world.resource::<Time>().elapsed();
        if world
            .resource::<StateTreeTimers<D>>()
            .active
            .values()
            .any(|timer| timer.deadline < now)
        {
            // Saturating multiple overdue deadlines to zero would combine
            // distinct timer cohorts. Capture only at the documented boundary.
            return Err(invalid(
                "process overdue state-tree timers before capturing a checkpoint",
            ));
        }
        let ingress = world.resource::<StateTreeEventIngress<D>>();
        let batch = ingress.batch.load(Ordering::Acquire);
        let broadcasts = ingress
            .broadcasts
            .lock()
            .expect("state-tree broadcast inbox poisoned")
            .iter()
            .map(|event| save_event(event, world))
            .collect::<Result<Vec<_>, _>>()?;
        let mut trees = Vec::new();
        let mut fingerprints = HashMap::new();
        for (entity, instance, inbox, outcomes, disabled) in query_all::<
            (
                Entity,
                &StateTreeInstance<D>,
                &StateTreeEventInbox<D>,
                Option<&StateTreeActivityInbox<D>>,
                Option<&bevy_ecs::entity_disabling::Disabled>,
            ),
            (),
        >(world)
        .iter(world)
        {
            let linked = instance.persistent_definition();
            let definition = linked
                .map(|linked| linked.compiled.name.to_string())
                .or_else(|| {
                    world
                        .resource::<Assets<StateTreeDefinition>>()
                        .get(instance.definition())
                        .map(|d| d.name().into())
                })
                .or_else(|| instance.definition().path().map(ToString::to_string))
                .ok_or_else(|| invalid("pending generated definition is unavailable"))?;
            let timers = world.resource::<StateTreeTimers<D>>();
            let runtime = instance.persistent_runtime(|token| {
                timers
                    .active
                    .get(&token)
                    .map(|timer| timer.deadline.saturating_sub(now))
            });
            let fingerprint = if let Some(linked) = linked {
                let value = match fingerprints.entry(linked.compiled.identity) {
                    std::collections::hash_map::Entry::Occupied(entry) => *entry.get(),
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        *entry.insert(fingerprint(&linked.compiled)?)
                    }
                };
                Some(value)
            } else {
                None
            };
            let events = inbox
                .events
                .lock()
                .expect("state-tree event inbox poisoned")
                .iter()
                .filter(|entry| entry.batch == batch && entry.instance == instance.runtime_id())
                .map(|entry| Ok((entry.broadcasts_before, save_event(&entry.event, world)?)))
                .collect::<Result<_, Error>>()?;
            let outcomes = outcomes
                .map(|inbox| {
                    inbox
                        .outcomes
                        .lock()
                        .expect("state-tree activity inbox poisoned")
                        .iter()
                        .filter(|outcome| instance.accepts_activity_token(outcome.token))
                        .map(|outcome| {
                            Ok(SavedOutcome {
                                run: run_id(outcome.token)?,
                                success: matches!(outcome.kind, ActivityOutcomeKind::Complete),
                                data: outcome
                                    .data
                                    .as_ref()
                                    .map(|v| v.as_ref().map(Value::from))
                                    .map_err(ToString::to_string),
                            })
                        })
                        .collect::<Result<Vec<_>, Error>>()
                })
                .transpose()?
                .unwrap_or_default();
            trees.push(TreeSnapshot {
                owner: entity_id(entity)?,
                disabled: disabled.is_some(),
                definition,
                path: instance.definition().path().map(ToString::to_string),
                fingerprint,
                runtime,
                events,
                outcomes,
            });
        }
        trees.sort_by_key(|tree| tree.owner);
        Ok(DomainSnapshot { trees, broadcasts })
    }

    fn prepare(
        &self,
        world: &mut World,
        snapshot: &DomainSnapshot,
        entities: &[Entity],
    ) -> Result<(Apply, Vec<ResolvedRun>, Vec<u64>), Error> {
        let now = world.resource::<Time>().elapsed();
        let mut prepared = Vec::new();
        let mut runs = Vec::new();
        let mut runtimes = Vec::new();
        let mut owners = std::collections::HashSet::new();
        let mut definitions = HashMap::<
            _,
            (
                bevy_asset::Handle<StateTreeDefinition>,
                Option<Arc<LinkedDefinition<D>>>,
            ),
        >::new();
        for tree in &snapshot.trees {
            let entity = *entities
                .get(tree.owner)
                .ok_or_else(|| invalid("invalid tree owner"))?;
            if !owners.insert(entity) {
                return Err(invalid("duplicate tree owner"));
            }
            let key = (tree.definition.clone(), tree.path.clone(), tree.fingerprint);
            let (handle, linked) = if let Some(value) = definitions.get(&key) {
                value.clone()
            } else {
                let handle = if let Some(path) = &tree.path {
                    let handle = world
                        .resource::<AssetServer>()
                        .load::<StateTreeDefinition>(path.clone());
                    if tree.fingerprint.is_some()
                        && !world
                            .resource::<Assets<StateTreeDefinition>>()
                            .contains(handle.id())
                    {
                        return Err(Error::DefinitionNotLoaded(path.clone()));
                    }
                    handle
                } else {
                    let id = {
                        let assets = world.resource::<Assets<StateTreeDefinition>>();
                        let mut candidates =
                            assets.iter().filter(|(_, d)| d.name() == tree.definition);
                        candidates
                            .find_map(|(id, d)| match tree.fingerprint {
                                Some(expected)
                                    if fingerprint(&d.compiled).ok() != Some(expected) =>
                                {
                                    None
                                }
                                _ => Some(id),
                            })
                            .ok_or_else(|| Error::DefinitionNotLoaded(tree.definition.clone()))?
                    };
                    world
                        .resource_mut::<Assets<StateTreeDefinition>>()
                        .get_strong_handle(id)
                        .ok_or_else(|| invalid("definition handle is unavailable"))?
                };
                let linked = if let Some(expected) = tree.fingerprint {
                    let compiled = world
                        .resource::<Assets<StateTreeDefinition>>()
                        .get(&handle)
                        .ok_or_else(|| Error::DefinitionNotLoaded(tree.definition.clone()))?
                        .compiled
                        .clone();
                    if fingerprint(&compiled)? != expected {
                        return Err(invalid(format!("definition changed: {}", tree.definition)));
                    }
                    Some(Arc::new(
                        LinkedDefinition::new(
                            compiled,
                            world.resource::<StateTreeAppEventRegistry<D>>().protocol(),
                            world.resource::<StateTreeEffectRegistry<D>>(),
                            world.resource::<StateTreeActivityRegistry<D>>(),
                        )
                        .map_err(|e| invalid(e.to_string()))?,
                    ))
                } else {
                    None
                };
                definitions.insert(key, (handle.clone(), linked.clone()));
                (handle, linked)
            };
            let instance = StateTreeInstance::<D>::from_persistent_runtime(
                handle,
                linked,
                &tree.runtime,
                now,
            )?;
            runtimes.push(instance.runtime_id());
            runs.extend(instance.persistent_runs(entity));
            prepared.push((entity, instance, tree.disabled));
        }
        Ok((
            Box::new(move |world| {
                for (entity, mut instance, disabled) in prepared {
                    let updates = instance.drain_timer_updates().collect::<Vec<_>>();
                    let initialized = instance.status() != &InstanceStatus::Pending;
                    let has_activities = instance
                        .persistent_definition()
                        .is_some_and(|linked| !linked.compiled.activities.is_empty());
                    let mut entity_mut = world.entity_mut(entity);
                    entity_mut.insert(instance);
                    if disabled {
                        entity_mut.insert(bevy_ecs::entity_disabling::Disabled);
                    }
                    if initialized {
                        entity_mut.remove::<PendingStateTree<D>>();
                    }
                    if has_activities {
                        entity_mut.insert(StateTreeActivityInbox::<D>::default());
                    }
                    for update in updates {
                        world
                            .resource_mut::<StateTreeTimers<D>>()
                            .apply(entity, update);
                    }
                }
            }),
            runs,
            runtimes,
        ))
    }

    fn prepare_ingress(
        &self,
        world: &World,
        snapshot: &DomainSnapshot,
        entities: &[Entity],
        runtimes: &[u64],
    ) -> Result<Apply, Error> {
        let protocol = world.resource::<StateTreeAppEventRegistry<D>>().protocol();
        let broadcasts = snapshot
            .broadcasts
            .iter()
            .map(|event| QueuedStateTreeEvent::from_persistent_snapshot(event, protocol))
            .collect::<Result<Vec<_>, _>>()?;
        let mut prepared = Vec::new();
        for (tree, runtime) in snapshot.trees.iter().zip(runtimes) {
            let entity = entities[tree.owner];
            let mut previous = 0;
            let events = tree
                .events
                .iter()
                .map(|(before, event)| {
                    if *before > broadcasts.len() || *before < previous {
                        return Err(invalid("invalid broadcast/entity event ordering"));
                    }
                    previous = *before;
                    Ok(EntityEventEntry {
                        batch: 0,
                        instance: *runtime,
                        broadcasts_before: *before,
                        event: QueuedStateTreeEvent::from_persistent_snapshot(event, protocol)?,
                    })
                })
                .collect::<Result<Vec<_>, Error>>()?;
            let mut seen = std::collections::HashSet::new();
            let outcomes = tree
                .outcomes
                .iter()
                .map(|outcome| {
                    let run = loaded_run(outcome.run)?;
                    if run.domain != D::AUTHORED_NAME
                        || run.token.entity != entity
                        || !seen.insert(outcome.run)
                    {
                        return Err(invalid("invalid activity outcome owner/duplicate"));
                    }
                    Ok(ActivityOutcome {
                        token: run.token,
                        kind: if outcome.success {
                            ActivityOutcomeKind::Complete
                        } else {
                            ActivityOutcomeKind::Fail
                        },
                        data: outcome
                            .data
                            .clone()
                            .map(|v| v.map(Into::into))
                            .map_err(crate::StateTreeEventDataError::new),
                    })
                })
                .collect::<Result<Vec<_>, Error>>()?;
            prepared.push((entity, events, outcomes));
        }
        Ok(Box::new(move |world| {
            let ingress = world.resource_mut::<StateTreeEventIngress<D>>();
            ingress.batch.store(0, Ordering::Release);
            ingress
                .published_broadcasts
                .store(broadcasts.len(), Ordering::Release);
            *ingress
                .broadcasts
                .lock()
                .expect("state-tree broadcast inbox poisoned") = broadcasts;
            for (entity, events, outcomes) in prepared {
                if !events.is_empty() {
                    world
                        .resource::<StateTreeEventIngress<D>>()
                        .dirty_entities
                        .lock()
                        .unwrap()
                        .insert(entity);
                    *world
                        .get::<StateTreeEventInbox<D>>(entity)
                        .unwrap()
                        .events
                        .lock()
                        .unwrap() = events;
                }
                if !outcomes.is_empty() {
                    let ingress = world.resource::<StateTreeActivityIngress<D>>();
                    ingress.dirty.lock().unwrap().insert(entity);
                    ingress.dirty_hint.store(true, Ordering::Release);
                    *world
                        .get::<StateTreeActivityInbox<D>>(entity)
                        .unwrap()
                        .outcomes
                        .lock()
                        .unwrap() = outcomes;
                }
            }
        }))
    }

    fn clear(&self, world: &mut World) {
        // Removal queues cancellation messages for the outgoing session. They
        // must not be delivered into the newly restored consumer resources.
        let activities = world
            .resource::<StateTreeActivityRegistry<D>>()
            .handlers_for_persistence();
        for activity in activities {
            activity.clear_lifecycle(world);
        }
        let effects = world
            .resource::<StateTreeEffectRegistry<D>>()
            .handlers_for_persistence();
        for effect in effects {
            effect.clear_messages(world);
        }
        world.insert_resource(StateTreeEventIngress::<D>::default());
        world.insert_resource(StateTreeActivityIngress::<D>::default());
        world.insert_resource(StateTreeTimers::<D>::default());
    }
}
