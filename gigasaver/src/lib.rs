//! Optional storage adapter. The game registers its normal persistent data with
//! `bevy_state_tree::persistence::AppPersistenceExt`, then saves/loads its world
//! through [`GigasaverStore`]. No activity adapters or Gigasaver changes are needed.
//!
//! A complete checkpoint is stored as one resource record in an isolated backend
//! world. This deliberately uses Gigasaver as a storage engine: tree/game data
//! share the checkpoint's own identity namespace and restore transaction. It does
//! not independently register the same game entities with Gigasaver, or install
//! per-frame capture systems. Call `save` at completed game schedule boundaries.

use bevy_app::App;
use bevy_ecs::prelude::*;
use bevy_gigasaver::{AppSaveExt, SavePlugin, WorldSaveExt, resource};
use bevy_state_tree::persistence::Checkpoint;
use serde::{Deserialize, Serialize};
use std::path::Path;

pub use bevy_gigasaver::{LoadOutcome, SaveStats};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Storage(#[from] bevy_gigasaver::Error),
    #[error(transparent)]
    Checkpoint(#[from] bevy_state_tree::persistence::Error),
    #[error("successfully load the existing checkpoint before saving")]
    LoadRequired,
    #[error("save contains no state-tree checkpoint")]
    MissingCheckpoint,
}

#[derive(Resource, Default, Serialize, Deserialize)]
struct StoredCheckpoint(Option<Checkpoint>);

/// A SQLite-backed checkpoint store. It performs work only on explicit calls;
/// scheduling autosaves is independent of the state-tree execution schedule.
#[derive(Resource)]
pub struct GigasaverStore {
    backend: World,
    ready: bool,
}

impl GigasaverStore {
    /// Opens storage without changing the game world. An existing destination
    /// must be successfully restored with [`Self::load`] before overwriting it.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let mut app = App::new();
        app.add_plugins(SavePlugin::sqlite(path).load_on_start(false))
            .init_resource::<StoredCheckpoint>()
            .persist(resource::<StoredCheckpoint>("bevy_state_tree.checkpoint"));
        let outcome = app.world_mut().load()?;
        Ok(Self {
            backend: std::mem::take(app.world_mut()),
            ready: outcome == LoadOutcome::Missing,
        })
    }

    /// Captures and atomically commits the world's registered persistent data.
    pub fn save(&mut self, world: &mut World) -> Result<SaveStats, Error> {
        if !self.ready {
            return Err(Error::LoadRequired);
        }
        let checkpoint = Checkpoint::capture(world)?;
        self.backend
            .insert_resource(StoredCheckpoint(Some(checkpoint)));
        Ok(self.backend.save()?)
    }

    /// Loads and restores a checkpoint. On a missing file the game world is
    /// untouched. Asset/read/decode errors retain the existing game state and
    /// block writes until a successful load. Asset-loading failures can be retried.
    pub fn load(&mut self, world: &mut World) -> Result<LoadOutcome, Error> {
        self.ready = false;
        let outcome = self.backend.load()?;
        if outcome == LoadOutcome::Loaded {
            self.backend
                .get_resource::<StoredCheckpoint>()
                .and_then(|stored| stored.0.as_ref())
                .ok_or(Error::MissingCheckpoint)?
                .restore(world)?;
        }
        self.ready = true;
        Ok(outcome)
    }
}
