# bevy_state_tree

Asset-backed hierarchical statecharts for Bevy.

The crate currently provides the runtime foundation:

- immutable `StateTreeDefinition` assets;
- entity-local, domain-typed `StateTreeInstance` components;
- atomic, compound, parallel, and final states;
- hierarchical, eventless, and compound-completion transitions;
- Luau data expressions and transition guards;
- ordered SCXML `raise` and Luau `assign` actions on entry, exit, initial, and
  regular transitions;
- typed effects that construct and publish concrete Bevy messages;
- opt-in typed event payload projection through Luau `_event.data`;
- bounded iterative macrostep stabilization;
- deterministic transition sets across orthogonal regions;
- ordered per-instance entity and broadcast event queues;
- parallel event processing across independent instances.

Activities, behavior-tree composites, history, and invocation are planned on
top of this foundation.

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

Payloads can opt into Luau projection with `data = projector`. The projector is
called only when an executed expression reads `_event.data`; its portable value
is not used for Rust event routing or retained by the instance.

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

fn wake_data(_: &Wake) -> Result<LuauValue, StateTreeEventDataError> {
    Ok(LuauValue::Record(
        [(Box::from("urgent"), LuauValue::Boolean(true))].into(),
    ))
}

#[derive(StateTreeEvents)]
#[state_tree(domain = Simulation)]
enum SimulationEvents {
    #[state_tree(name = "wake", data = wake_data)]
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

## Typed Effects

Bevy-specific executable content uses the `urn:bevy-state-tree` namespace:

```xml
<scxml xmlns="http://www.w3.org/2005/07/scxml"
       xmlns:bst="urn:bevy-state-tree" datamodel="luau">
  <state id="active">
    <onentry>
      <bst:effect name="play_sound">
        <bst:arg name="volume" expr="0.75"/>
      </bst:effect>
    </onentry>
  </state>
</scxml>
```

Register each authored name with `StateTreeAppExt::register_state_tree_effect`.
The constructor consumes named `StateTreeEffectArgs` and returns one concrete
Bevy `Message`. Messages are available to systems in `StateTreeSet::HandleEffects`.
Construction follows statechart action order, but message delivery order between
parallel instances is unspecified. Constructor, argument, and payload projection
errors fault only the affected instance. Headless callers use
`StateTreeExecution::{new,process_event}_with_sink` to receive portable requests.

## Semantic Tests

Each case named in `tests/scxml/manifest.txt` has a project-owned `.scxml`
source and `.trace` golden log. An optional `.events` sidecar supplies one
external authored event name per line from the harness's closed event protocol.
The corpus runner reports every mismatch together so adding cases requires no
case-specific Rust assertions.

## Benchmarks

Run `just bench` to process broadcast events through Bevy's ECS and compute task
pool across as many as one million independent trees. The benchmark definitions
are the `benchmark_*.scxml` cases in `tests/scxml`, where their behavior is also
covered by the semantic trace corpus. Parsing, compilation, entity spawning,
instance initialization, and event queueing happen outside the timed region;
results report aggregate instance throughput for the scheduled AI processing.
