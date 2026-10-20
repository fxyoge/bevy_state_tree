//! State-tree's transaction participant. Ordinary resources/components and their
//! lifecycles are owned by Gigasaver, including backend-independent checkpoints.
pub use crate::instance::persistence::{
    RuntimeSnapshot, SavedActivity, SavedHistory, SavedState, SavedStateKind, SavedTimer,
};
use crate::{
    StateTreeActivity, StateTreeActivityToken, StateTreeDomain, activity::ErasedActivityToken,
};
use bevy_app::App;
use bevy_ecs::prelude::*;
pub(crate) use bevy_gigasaver::adapter::Apply;
pub use bevy_gigasaver::{AppSaveExt as AppPersistenceExt, Checkpoint, Error};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{
    any::TypeId,
    cell::RefCell,
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
};

pub(crate) fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidData(message.into())
}

/// Transform the symbolic state-tree payload inside a Gigasaver application migration.
/// The staged payload is replaced only when the transformation and encoding succeed.
/// Checkpoint-local run references must be preserved or remapped together with their
/// references in consumer data when changing live activities.
///
/// ```no_run
/// use bevy_gigasaver::SavePlugin;
/// use bevy_state_tree::persistence::migrate_runtime;
///
/// let saves = SavePlugin::default().version(2).migrate(1, |data| {
///     migrate_runtime(data, |snapshot| {
///         for domain in snapshot.domains.values_mut() {
///             for tree in &mut domain.trees {
///                 if let Some(value) = tree.runtime.data.remove("old_balance") {
///                     tree.runtime.data.insert("balance".into(), value);
///                 }
///             }
///         }
///         Ok(())
///     })
/// });
/// ```
pub fn migrate_runtime(
    data: &mut bevy_gigasaver::MigrationData,
    migrate: impl FnOnce(&mut RuntimeCheckpoint) -> Result<(), String>,
) -> Result<(), String> {
    let Some(saved) = data.get_mut("bevy_state_tree.runtime") else {
        return Ok(());
    };
    let bevy_gigasaver::SavedType::Resource(Some(value)) = saved else {
        return Err("invalid state-tree runtime migration payload".into());
    };
    let mut snapshot: RuntimeCheckpoint =
        serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
    if snapshot.format != 1 {
        return Err("unsupported state-tree snapshot format".into());
    }
    migrate(&mut snapshot)?;
    if snapshot.format != 1 {
        return Err("migration changed the state-tree format version".into());
    }
    *value = serde_json::to_value(snapshot).map_err(|e| e.to_string())?;
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuntimeCheckpoint {
    /// State-tree wire format, independent of the application's migration version.
    pub format: u32,
    /// Checkpoint-local identities. Removing a run must not renumber other runs.
    pub runs: BTreeMap<usize, RunReference>,
    pub domains: BTreeMap<String, DomainSnapshot>,
    pub data: BTreeMap<String, Vec<u8>>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunReference {
    pub owner: usize,
    pub domain: String,
    pub activity: String,
    pub generation: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DomainSnapshot {
    pub trees: Vec<TreeSnapshot>,
    pub broadcasts: Vec<SavedEvent>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TreeSnapshot {
    pub owner: usize,
    pub disabled: bool,
    pub definition: String,
    pub path: Option<String>,
    pub runtime: crate::instance::persistence::RuntimeSnapshot,
    pub events: Vec<(usize, SavedEvent)>,
    pub outcomes: Vec<SavedOutcome>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedEvent {
    pub name: String,
    pub payload: Vec<u8>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedOutcome {
    pub run: usize,
    pub success: bool,
    pub data: Result<Option<Value>, String>,
}
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
#[derive(Clone)]
pub(crate) struct ResolvedRun {
    pub token: ErasedActivityToken,
    pub domain: String,
    pub activity: String,
    pub activity_type: TypeId,
}
enum Session {
    Save {
        runs: HashMap<ErasedActivityToken, usize>,
    },
    Load {
        runs: BTreeMap<usize, ResolvedRun>,
    },
}
thread_local! { static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) }; }
struct Guard;
impl Drop for Guard {
    fn drop(&mut self) {
        SESSION.with(|s| s.replace(None));
    }
}
fn begin(session: Session) -> Result<Guard, Error> {
    if SESSION.with(|s| s.borrow().is_some()) {
        return Err(invalid("nested state-tree save context"));
    }
    SESSION.with(|s| s.replace(Some(session)));
    Ok(Guard)
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
pub(crate) fn run_id(token: ErasedActivityToken) -> Result<usize, Error> {
    SESSION.with(|s| match s.borrow().as_ref() {
        Some(Session::Save { runs }) => runs
            .get(&token)
            .copied()
            .ok_or_else(|| invalid("activity token does not identify a saved live run")),
        _ => Err(invalid("token serialization requires a save context")),
    })
}
pub(crate) fn loaded_run(id: usize) -> Result<ResolvedRun, Error> {
    SESSION.with(|s| match s.borrow().as_ref() {
        Some(Session::Load { runs, .. }) => runs
            .get(&id)
            .cloned()
            .ok_or_else(|| invalid("dangling activity run reference")),
        _ => Err(invalid("token deserialization requires a restore context")),
    })
}
impl<D: StateTreeDomain, A: StateTreeActivity> Serialize for StateTreeActivityToken<D, A> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(run_id(self.erased()).map_err(serde::ser::Error::custom)? as u64)
    }
}
impl<'de, D: StateTreeDomain, A: StateTreeActivity> Deserialize<'de>
    for StateTreeActivityToken<D, A>
{
    fn deserialize<S: Deserializer<'de>>(deserializer: S) -> Result<Self, S::Error> {
        let run =
            loaded_run(usize::deserialize(deserializer)?).map_err(serde::de::Error::custom)?;
        if run.domain != D::AUTHORED_NAME || run.activity_type != TypeId::of::<A>() {
            return Err(serde::de::Error::custom(
                "activity run domain/type mismatch",
            ));
        }
        Ok(run.token.typed())
    }
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
pub(crate) trait DomainCodec: Send + Sync {
    fn entities(&self, world: &mut World) -> Vec<Entity>;
    fn runs(&self, world: &mut World) -> Vec<ResolvedRun>;
    fn capture(&self, world: &mut World) -> Result<DomainSnapshot, Error>;
    fn prepare(
        &self,
        world: &mut World,
        snapshot: &DomainSnapshot,
    ) -> Result<(Apply, Vec<ResolvedRun>, Vec<u64>), Error>;
    fn prepare_ingress(
        &self,
        world: &World,
        snapshot: &DomainSnapshot,
        runtimes: &[u64],
    ) -> Result<Apply, Error>;
    fn clear(&self, world: &mut World);
}
pub(crate) trait DataCodec: Send + Sync {
    fn clear(&self, _world: &mut World) {}
    fn capture(&self, world: &mut World) -> Result<Vec<u8>, Error>;
    fn prepare(&self, world: &World, bytes: &[u8]) -> Result<Apply, Error>;
}
#[derive(Resource, Default)]
pub(crate) struct Registry {
    pub domains: BTreeMap<String, Arc<dyn DomainCodec>>,
    pub data: BTreeMap<String, Arc<dyn DataCodec>>,
    pending_definitions: Vec<bevy_asset::Handle<crate::StateTreeDefinition>>,
}
pub(crate) fn register_data(app: &mut App, key: &str, codec: Arc<dyn DataCodec>) {
    ensure_adapter(app);
    assert!(
        app.world_mut()
            .resource_mut::<Registry>()
            .data
            .insert(key.into(), codec)
            .is_none(),
        "duplicate state-tree runtime key: {key}"
    );
}
pub(crate) fn ensure_adapter(app: &mut App) {
    if !app.world().contains_resource::<Registry>() {
        app.init_resource::<Registry>();
        bevy_gigasaver::adapter::register(app, "bevy_state_tree.runtime", RuntimeAdapter);
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
struct RuntimeAdapter;
impl bevy_gigasaver::adapter::SaveAdapter for RuntimeAdapter {
    fn entities(&self, world: &mut World) -> Vec<Entity> {
        let domains = world.resource::<Registry>().domains.clone();
        domains
            .values()
            .flat_map(|codec| codec.entities(world))
            .collect()
    }
    fn capture(&self, world: &mut World) -> Result<bevy_gigasaver::adapter::Captured, Error> {
        let domains = world.resource::<Registry>().domains.clone();
        let codecs = world.resource::<Registry>().data.clone();
        let mut runs = BTreeMap::new();
        let mut tokens = HashMap::new();
        for (domain, codec) in &domains {
            for run in codec.runs(world) {
                let token = run.token;
                let id = runs.len();
                tokens.insert(token, id);
                runs.insert(
                    id,
                    RunReference {
                        owner: entity_id(token.entity)?,
                        domain: domain.clone(),
                        activity: run.activity,
                        generation: token.generation,
                    },
                );
            }
        }
        let guard = begin(Session::Save { runs: tokens })?;
        let domains = domains
            .iter()
            .map(|(key, codec)| Ok((key.clone(), codec.capture(world)?)))
            .collect::<Result<_, Error>>()?;
        let data = codecs
            .iter()
            .map(|(key, codec)| Ok((key.clone(), codec.capture(world)?)))
            .collect::<Result<_, Error>>()?;
        Ok(bevy_gigasaver::adapter::Captured {
            bytes: serde_json::to_vec(&RuntimeCheckpoint {
                format: 1,
                runs,
                domains,
                data,
            })?,
            context: Box::new(guard),
        })
    }
    fn prepare(
        &self,
        world: &mut World,
        bytes: &[u8],
    ) -> Result<bevy_gigasaver::adapter::Prepared, Error> {
        let snapshot: RuntimeCheckpoint = serde_json::from_slice(bytes)?;
        let domains = world.resource::<Registry>().domains.clone();
        let codecs = world.resource::<Registry>().data.clone();
        if snapshot.format != 1 {
            return Err(invalid("unsupported state-tree snapshot format"));
        }
        if snapshot
            .domains
            .keys()
            .any(|key| !domains.contains_key(key))
            || snapshot.data.keys().any(|key| !codecs.contains_key(key))
        {
            return Err(invalid(
                "migration required: removed state-tree domain or runtime codec",
            ));
        }
        if let Some(server) = world.get_resource::<bevy_asset::AssetServer>() {
            let paths: std::collections::BTreeSet<_> = snapshot
                .domains
                .values()
                .flat_map(|domain| &domain.trees)
                .filter_map(|tree| tree.path.clone())
                .collect();
            let handles = paths.into_iter().map(|path| server.load(path)).collect();
            world.resource_mut::<Registry>().pending_definitions = handles;
        }
        let mut apply = Vec::new();
        let mut live = HashMap::new();
        let mut runtime_ids = BTreeMap::new();
        for (key, codec) in &domains {
            let empty = DomainSnapshot {
                trees: Vec::new(),
                broadcasts: Vec::new(),
            };
            let (prepared, runs, runtimes) =
                codec.prepare(world, snapshot.domains.get(key).unwrap_or(&empty))?;
            apply.push(prepared);
            runtime_ids.insert(key.clone(), runtimes);
            for run in runs {
                let key = (
                    run.domain.clone(),
                    run.token.entity,
                    run.activity.clone(),
                    run.token.generation,
                );
                if live.insert(key, run).is_some() {
                    return Err(invalid("duplicate restored activity"));
                }
            }
        }
        let mut seen = HashSet::new();
        let runs = snapshot
            .runs
            .iter()
            .map(|(id, reference)| {
                let key = (
                    reference.domain.clone(),
                    loaded_entity(reference.owner)?,
                    reference.activity.clone(),
                    reference.generation,
                );
                if !seen.insert(key.clone()) {
                    return Err(invalid("duplicate activity run reference"));
                }
                let run = live
                    .get(&key)
                    .ok_or_else(|| invalid("reference to an inactive activity"))?;
                Ok((*id, run.clone()))
            })
            .collect::<Result<BTreeMap<_, _>, Error>>()?;
        if runs.len() != live.len() {
            return Err(invalid("missing activity run reference"));
        }
        let guard = begin(Session::Load { runs })?;
        for (key, codec) in &codecs {
            if let Some(bytes) = snapshot.data.get(key) {
                apply.push(codec.prepare(world, bytes)?);
            }
        }
        for (key, codec) in &domains {
            let empty = DomainSnapshot {
                trees: Vec::new(),
                broadcasts: Vec::new(),
            };
            apply.push(codec.prepare_ingress(
                world,
                snapshot.domains.get(key).unwrap_or(&empty),
                &runtime_ids[key],
            )?);
        }
        Ok(bevy_gigasaver::adapter::Prepared {
            entities: snapshot
                .domains
                .values()
                .flat_map(|domain| &domain.trees)
                .map(|tree| loaded_entity(tree.owner))
                .collect::<Result<_, _>>()?,
            apply: Box::new(move |world| {
                for apply in apply {
                    apply(world);
                }
                world.resource_mut::<Registry>().pending_definitions.clear();
            }),
            context: Box::new(guard),
        })
    }
    fn clear(&self, world: &mut World) {
        let domains = world.resource::<Registry>().domains.clone();
        let codecs = world.resource::<Registry>().data.clone();
        for codec in domains.values() {
            codec.clear(world);
        }
        for codec in codecs.values() {
            codec.clear(world);
        }
        world.resource_mut::<Registry>().pending_definitions.clear();
    }
}

#[doc(hidden)]
pub mod __private {
    pub use serde;
    pub use serde_json;
}
