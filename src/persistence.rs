//! Only the datamodel persists. Execution is always rebuilt from current assets.
use bevy_app::App;
use bevy_ecs::prelude::*;
pub(crate) use bevy_gigasaver::adapter::Apply;
pub use bevy_gigasaver::{AppSaveExt as AppPersistenceExt, Checkpoint, Error};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;

pub(crate) fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidData(message.into())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    format: u32,
    trees: Vec<TreeSnapshot>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TreeSnapshot {
    pub owner: usize,
    pub definition: String,
    pub path: Option<String>,
    pub data: BTreeMap<Box<str>, Value>,
}

/// Lossless portable datamodel values, including nonfinite floating point values.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Value {
    Nil,
    Boolean(bool),
    Integer(i64),
    Number(u64),
    String(String),
    Sequence(Vec<Value>),
    Record(BTreeMap<String, Value>),
}
impl From<&crate::LuauValue> for Value {
    fn from(value: &crate::LuauValue) -> Self {
        use crate::LuauValue as L;
        match value {
            L::Nil => Self::Nil,
            L::Boolean(v) => Self::Boolean(*v),
            L::Integer(v) => Self::Integer(*v),
            L::Number(v) => Self::Number(v.to_bits()),
            L::String(v) => Self::String(v.clone()),
            L::Sequence(v) => Self::Sequence(v.iter().map(Self::from).collect()),
            L::Record(v) => {
                Self::Record(v.iter().map(|(k, v)| (k.clone(), Self::from(v))).collect())
            }
        }
    }
}
impl From<Value> for crate::LuauValue {
    fn from(value: Value) -> Self {
        match value {
            Value::Nil => Self::Nil,
            Value::Boolean(v) => Self::Boolean(v),
            Value::Integer(v) => Self::Integer(v),
            Value::Number(v) => Self::Number(f64::from_bits(v)),
            Value::String(v) => Self::String(v),
            Value::Sequence(v) => Self::Sequence(v.into_iter().map(Self::from).collect()),
            Value::Record(v) => {
                Self::Record(v.into_iter().map(|(k, v)| (k, Self::from(v))).collect())
            }
        }
    }
}

pub(crate) fn entity_id(entity: Entity) -> Result<usize, Error> {
    usize::try_from(bevy_gigasaver::entity::saved_id(entity)?)
        .map_err(|_| invalid("invalid entity identity"))
}
pub(crate) fn loaded_entity(id: usize) -> Result<Entity, Error> {
    bevy_gigasaver::entity::loaded_entity(
        i64::try_from(id).map_err(|_| invalid("invalid entity identity"))?,
    )
}
pub use bevy_gigasaver::asset::optional as optional_asset;
pub use bevy_gigasaver::entity::{
    list as entities, map as entity_map, optional as optional_entity,
};
pub use bevy_gigasaver::{asset, entity};
pub mod luau_value {
    use super::*;
    pub fn serialize<S: Serializer>(
        value: &crate::LuauValue,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        Value::from(value).serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<crate::LuauValue, D::Error> {
        Ok(Value::deserialize(deserializer)?.into())
    }
}

#[derive(Resource, Default)]
pub(crate) struct Registry {
    pending_definitions: Vec<bevy_asset::Handle<crate::StateTreeDefinition>>,
}
pub(crate) fn ensure_adapter(app: &mut App) {
    if !app.world().contains_resource::<Registry>() {
        app.init_resource::<Registry>();
        bevy_gigasaver::adapter::register(app, "bevy_state_tree.datamodel", DatamodelAdapter);
    }
}
pub(crate) fn query_all<Q: bevy_ecs::query::QueryData, F: bevy_ecs::query::QueryFilter>(
    world: &mut World,
) -> bevy_ecs::query::QueryState<Q, F> {
    use bevy_ecs::entity_disabling::DefaultQueryFilters;
    if world.contains_resource::<DefaultQueryFilters>() {
        world.resource_scope(|world, _: Mut<DefaultQueryFilters>| world.query_filtered::<Q, F>())
    } else {
        world.query_filtered::<Q, F>()
    }
}
struct DatamodelAdapter;
impl bevy_gigasaver::adapter::SaveAdapter for DatamodelAdapter {
    fn entities(&self, world: &mut World) -> Vec<Entity> {
        crate::plugin::persistence::entities(world)
    }
    fn capture(&self, world: &mut World) -> Result<bevy_gigasaver::adapter::Captured, Error> {
        let trees = crate::plugin::persistence::capture(world)?;
        Ok(bevy_gigasaver::adapter::Captured {
            bytes: serde_json::to_vec(&Snapshot { format: 2, trees })?,
            context: Box::new(()),
        })
    }
    fn prepare(
        &self,
        world: &mut World,
        bytes: &[u8],
    ) -> Result<bevy_gigasaver::adapter::Prepared, Error> {
        // Check the version before deserializing the payload so an old save
        // produces an explicit format error, even though its shape differs.
        #[derive(Deserialize)]
        struct Version {
            format: u32,
        }
        if serde_json::from_slice::<Version>(bytes)?.format != 2 {
            return Err(invalid("unsupported datamodel snapshot format"));
        }
        let snapshot: Snapshot = serde_json::from_slice(bytes)?;
        if let Some(server) = world.get_resource::<bevy_asset::AssetServer>() {
            let handles = snapshot
                .trees
                .iter()
                .filter_map(|tree| tree.path.as_ref())
                .map(|path| server.load(path.clone()))
                .collect();
            world.resource_mut::<Registry>().pending_definitions = handles;
        }
        let apply = crate::plugin::persistence::prepare(world, &snapshot.trees)?;
        let entities = snapshot
            .trees
            .iter()
            .map(|tree| loaded_entity(tree.owner))
            .collect::<Result<_, _>>()?;
        Ok(bevy_gigasaver::adapter::Prepared {
            entities,
            apply: Box::new(move |world| {
                apply(world);
                world.resource_mut::<Registry>().pending_definitions.clear();
            }),
            context: Box::new(()),
        })
    }
    fn clear(&self, world: &mut World) {
        crate::plugin::persistence::clear(world);
        world.resource_mut::<Registry>().pending_definitions.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_gigasaver::adapter::SaveAdapter;

    #[test]
    fn old_domain_grouped_snapshots_are_rejected_before_world_mutation() {
        let mut world = World::new();
        let result =
            DatamodelAdapter.prepare(&mut world, br#"{"format":1,"domains":{"devices":[]}}"#);
        assert!(
            matches!(result, Err(Error::InvalidData(message)) if message == "unsupported datamodel snapshot format")
        );
        assert!(!world.contains_resource::<Registry>());
    }
}
