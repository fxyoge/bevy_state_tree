# bevy_state_tree

Asset-backed hierarchical statecharts for Bevy.

The crate currently provides the runtime foundation:

- immutable `StateTreeDefinition` assets;
- entity-local, domain-typed `StateTreeInstance` components;
- atomic, compound, and final states;
- hierarchical, eventless, and compound-completion transitions;
- Luau data expressions and transition guards;
- ordered SCXML `raise` and Luau `assign` actions on entry, exit, initial, and
  regular transitions;
- bounded iterative macrostep stabilization;
- ordered per-instance entity and broadcast event queues;
- parallel event processing across independent instances.

Activities, behavior-tree composites, parallel regions, history, and invocation
are planned on top of this foundation.

The `scxml` module provides a namespace-aware authored-syntax parser that keeps
SCXML structure, document order, source spans, raw expressions, and foreign
extensions separate from the runtime representation. Its compiler accepts only
semantics implemented by the production runtime and rejects unsupported syntax
instead of ignoring it. Compiled definitions contain dense identifiers and
precomputed transition entry/exit plans; macrosteps do not traverse the SCXML
tree or resolve authored names.

See [SCXML Support Policy](SCXML.md) for the Bevy-first compatibility scope and
intentional differences from general-purpose SCXML processors.

Project-owned SCXML fixtures under `tests/scxml` execute through the public
headless runtime and are compared with deterministic semantic event logs. The
same opt-in tracing API is available to applications without adding tracing
work to the normal Bevy execution path.

Each runtime domain owns a closed external event protocol. Tuple variants keep
the original Rust payload intact through queueing and execution, while the
derive generates event-name matching and writer conversions. Variants are
entity-targeted by default; mark domain-wide events with `broadcast`.

```rust,no_run
use bevy_app::{App, FixedUpdate};
use bevy_asset::AssetPlugin;
use bevy_ecs::prelude::*;
use bevy_state_tree::prelude::*;

struct Simulation;

#[derive(EntityEvent)]
struct Wake {
    entity: Entity,
}

#[derive(StateTreeEvents)]
#[state_tree(domain = Simulation)]
enum SimulationEvents {
    #[state_tree(name = "wake")]
    Wake(Wake),
}

fn wake(mut events: EntityStateTreeEventWriter<Simulation>) {
    // In a real system this would be the entity whose tree should wake.
    let entity = Entity::PLACEHOLDER;
    events.write(Wake { entity });
}

let mut app = App::new();
app.add_plugins(AssetPlugin::default())
    .add_plugins(StateTreePlugin::<Simulation>::new(FixedUpdate))
    .add_systems(FixedUpdate, wake.in_set(StateTreeSet::QueueEvents));
```

Each state-tree instance observes its own ordered event stream. Entity events
for different instances have no relative order and those instances are
processed in parallel. Broadcast events are merged into each instance's stream
at the point where they were written.

Instances that initialize before event processing receive events queued during
that schedule run. Instances still waiting for their definition asset receive
no events, and queued entity events are discarded rather than replayed later.

## Semantic Tests

Each case named in `tests/scxml/manifest.txt` has a project-owned `.scxml`
source and `.trace` golden log. An optional `.events` sidecar supplies one
external authored event name per line from the harness's closed event protocol.
The corpus runner reports every mismatch together so adding cases requires no
case-specific Rust assertions.

## Benchmarks

Run `just bench` for initialization, hierarchy depth, eventless stabilization,
transition fanout, ignored and handled external events, Luau execution, and
opt-in tracing. Divan prepares each mutable execution outside the timed region
and defers teardown until after timing; initialization benchmarks intentionally
include linking and runtime storage setup.
