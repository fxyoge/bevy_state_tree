//! End-to-end synchronous checkpoint cost, including SQLite FULL durability.
//! This is a payload-size probe, not a substitute for profiling the consuming game.
//! Run with `cargo bench --features persistence --bench checkpoint_frames`.
use bevy_app::{App, TaskPoolPlugin, Update};
use bevy_asset::{AssetPlugin, Assets};
use bevy_ecs::prelude::*;
use bevy_gigasaver::{SavePlugin, WorldSaveExt};
use bevy_state_tree::{
    persistence::{AppPersistenceExt, Checkpoint},
    prelude::*,
};
use bevy_time::TimePlugin;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

fn configure(_: &mut StateTreeConfig<'_>) {}

#[derive(Component, Serialize, Deserialize)]
struct Sheet {
    ink: Vec<bool>,
    stage: Vec<bool>,
}

fn app(sheets: usize) -> App {
    let mut app = App::new();
    app.add_plugins((
        TaskPoolPlugin::default(),
        TimePlugin,
        AssetPlugin::default(),
        StateTreePlugin::new(Update).configure(configure),
    ))
    .persist_component::<Sheet>("profile.sheet");
    let mut builder = StateTreeBuilder::new("profile-tree");
    let root = builder.compound_root("root").unwrap();
    let idle = builder.atomic("idle", root).unwrap();
    builder.initial(root, idle).unwrap();
    let definition = app
        .world_mut()
        .resource_mut::<Assets<StateTreeDefinition>>()
        .add(builder.build().unwrap());
    for _ in 0..32 {
        app.world_mut()
            .spawn(StateTreeInstance::new(definition.clone()));
    }
    for _ in 0..sheets {
        app.world_mut().spawn((
            StateTreeInstance::new(definition.clone()),
            Sheet {
                ink: (0..1600).map(|i| i % 3 == 0).collect(),
                stage: vec![false; 1600],
            },
        ));
    }
    app.update();
    app
}

fn report(sheets: usize, phase: &str, samples: &mut [Duration]) {
    samples.sort_unstable();
    let ms = |n: usize| samples[(samples.len() - 1) * n / 100].as_secs_f64() * 1000.0;
    println!(
        "sheets={sheets} phase={phase} samples={} p50_ms={:.3} p95_ms={:.3} p99_ms={:.3} max_ms={:.3}",
        samples.len(),
        ms(50),
        ms(95),
        ms(99),
        ms(100),
    );
}

fn main() {
    for sheets in [0, 64, 512] {
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(sheets);
        app.add_plugins(
            SavePlugin::sqlite(directory.path().join("profile.sqlite")).load_on_start(false),
        );
        app.world_mut().save().unwrap();
        let mut idle = Vec::new();
        let mut saves = Vec::new();
        let mut captures = Vec::new();
        let mut encodes = Vec::new();
        let mut payload_bytes = 0;
        // Setup and the initial commit are excluded; save frames are reported
        // separately so a rare stall cannot hide in the overall median.
        for frame in 0..1200 {
            let start = Instant::now();
            app.update();
            if frame % 60 == 0 {
                let stats = app.world_mut().save().unwrap();
                saves.push(start.elapsed());
                payload_bytes = stats.bytes;
                let start = Instant::now();
                let checkpoint = Checkpoint::capture(app.world_mut()).unwrap();
                captures.push(start.elapsed());
                let start = Instant::now();
                std::hint::black_box(serde_json::to_vec(&checkpoint).unwrap());
                encodes.push(start.elapsed());
            } else {
                idle.push(start.elapsed());
            }
        }
        println!("sheets={sheets} payload_bytes={payload_bytes}");
        report(sheets, "idle-update", &mut idle);
        report(sheets, "save-update", &mut saves);
        report(sheets, "capture", &mut captures);
        report(sheets, "outer-encode", &mut encodes);
    }
}
