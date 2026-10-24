use super::*;
use crate::persistence::{
    Apply, Error, TreeSnapshot, Value, entity_id, invalid, loaded_entity, query_all,
};
use bevy_ecs::{prelude::Without, world::World};

pub(crate) fn entities(world: &mut World) -> Vec<Entity> {
    query_all::<
        Entity,
        (
            With<StateTreeInstance>,
            Without<crate::spawn_activity::SpawnedStateTree>,
        ),
    >(world)
    .iter(world)
    .collect()
}

pub(crate) fn capture(world: &mut World) -> Result<Vec<TreeSnapshot>, Error> {
    let mut trees = Vec::new();
    for (entity, instance) in query_all::<
        (Entity, &StateTreeInstance),
        Without<crate::spawn_activity::SpawnedStateTree>,
    >(world)
    .iter(world)
    {
        let definition = world
            .resource::<Assets<StateTreeDefinition>>()
            .get(instance.definition())
            .map(|asset| asset.name().to_string())
            .or_else(|| instance.definition().path().map(ToString::to_string))
            .ok_or_else(|| invalid("generated definition is unavailable"))?;
        trees.push(TreeSnapshot {
            owner: entity_id(entity)?,
            definition,
            path: instance.definition().path().map(ToString::to_string),
            data: instance
                .datamodel_values()
                .into_iter()
                .map(|(name, value)| (name, Value::from(&value)))
                .collect(),
        });
    }
    trees.sort_by_key(|tree| tree.owner);
    Ok(trees)
}

pub(crate) fn prepare(world: &mut World, trees: &[TreeSnapshot]) -> Result<Apply, Error> {
    let mut prepared = Vec::new();
    let mut owners = std::collections::HashSet::new();
    for tree in trees {
        let entity = loaded_entity(tree.owner)?;
        if !owners.insert(entity) {
            return Err(invalid("duplicate tree owner"));
        }
        let handle = if let Some(path) = &tree.path {
            world
                .resource::<AssetServer>()
                .load::<StateTreeDefinition>(path.clone())
        } else {
            let id = {
                let assets = world.resource::<Assets<StateTreeDefinition>>();
                let mut candidates = assets
                    .iter()
                    .filter(|(_, asset)| asset.name() == tree.definition);
                let id = candidates
                    .next()
                    .map(|(id, _)| id)
                    .ok_or_else(|| Error::AssetNotReady(tree.definition.clone()))?;
                if candidates.next().is_some() {
                    return Err(invalid("ambiguous generated definition"));
                }
                id
            };
            world
                .resource_mut::<Assets<StateTreeDefinition>>()
                .get_strong_handle(id)
                .ok_or_else(|| invalid("definition handle unavailable"))?
        };
        prepared.push((
            entity,
            StateTreeInstance::new(handle).with_preserved_data(
                tree.data
                    .iter()
                    .map(|(name, value)| (name.clone(), value.clone().into()))
                    .collect(),
            ),
        ));
    }
    Ok(Box::new(move |world| {
        for (entity, instance) in prepared {
            world.entity_mut(entity).insert(instance);
        }
    }))
}

pub(crate) fn clear(world: &mut World) {
    // Retain cancellation delivery so transient consumers release run-owned objects.
    for effect in world
        .resource::<StateTreeEffectRegistry>()
        .handlers_for_persistence()
    {
        effect.clear_messages(world);
    }
    world.resource_scope(|world, registry: Mut<StateTreeActivityRegistry>| {
        registry.discard_pending_starts(world)
    });
    world.insert_resource(StateTreeEventIngress::default());
    world.insert_resource(StateTreeActivityIngress::default());
    world.insert_resource(StateTreeTimers::default());
}
