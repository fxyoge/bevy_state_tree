use super::*;
use crate::persistence::{Apply, DataCodec, Error, invalid, register_data};
use bevy_ecs::world::World;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(bound = "")]
struct SavedRun<D: StateTreeDomain> {
    token: StateTreeActivityToken<D, SpawnActivity>,
    definition: String,
    #[serde(with = "crate::persistence::optional_entity")]
    child: Option<Entity>,
}

struct Codec<D>(PhantomData<fn() -> D>);

pub(super) fn register<D: StateTreeDomain>(app: &mut App) {
    register_data(
        app,
        &format!("bst.spawn.{}", D::AUTHORED_NAME),
        std::sync::Arc::new(Codec::<D>(PhantomData)),
    );
}

impl<D: StateTreeDomain> DataCodec for Codec<D> {
    fn capture(&self, world: &mut World) -> Result<Vec<u8>, Error> {
        let runs = world
            .resource::<SpawnRuns<D>>()
            .runs
            .iter()
            .map(|run| SavedRun::<D> {
                token: run.token,
                definition: run.definition.clone(),
                child: match run.state {
                    SpawnRunState::Loading(_) => None,
                    SpawnRunState::Running(child) => Some(child),
                },
            })
            .collect::<Vec<_>>();
        Ok(serde_json::to_vec(&runs)?)
    }

    fn prepare(&self, world: &World, bytes: &[u8]) -> Result<Apply, Error> {
        let saved: Vec<SavedRun<D>> = serde_json::from_slice(bytes)?;
        let mut runs = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for run in saved {
            if !seen.insert(run.token) {
                return Err(invalid("duplicate spawned-tree activity"));
            }
            let path = AssetPath::try_parse(&run.definition).map_err(|e| invalid(e.to_string()))?;
            if path.path().extension().and_then(|v| v.to_str()) != Some("scxml") {
                return Err(invalid("invalid spawned-tree definition path"));
            }
            let state = match run.child {
                Some(child) => SpawnRunState::Running(child),
                None => {
                    let server = world.resource::<AssetServer>();
                    SpawnRunState::Loading(
                        server
                            .get_handle(&run.definition)
                            .unwrap_or_else(|| server.load(run.definition.clone())),
                    )
                }
            };
            runs.push(SpawnRun {
                token: run.token,
                definition: run.definition,
                state,
            });
        }
        Ok(Box::new(move |world| {
            world.insert_resource(SpawnRuns::<D> {
                runs,
                marker: PhantomData,
            });
        }))
    }
}
