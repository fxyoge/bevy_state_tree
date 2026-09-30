# bevy_state_tree

Asset-backed hierarchical statecharts for Bevy.

The crate currently provides the runtime foundation:

- immutable `StateTreeDefinition` assets;
- entity-local, domain-typed `StateTreeInstance` components;
- atomic, compound, parallel, and final states;
- hierarchical, eventless, and compound-completion transitions;
- static state-scoped `after` transitions with deterministic deadline ordering;
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

`StateTreePlugin` registers an asset loader for `.scxml` files. Loading parses
and compiles the source once into the same `StateTreeDefinition` asset type
produced by `StateTreeBuilder`; no authored syntax is retained or interpreted at
runtime. Definitions generated in code remain supported through
`Assets<StateTreeDefinition>::add`.

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
use bevy_time::TimePlugin;

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
app.add_plugins((TimePlugin, AssetPlugin::default()))
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

## Loading Definitions

After installing `StateTreePlugin` for a domain, load an SCXML definition through
Bevy's normal asset server and attach its handle to an entity:

```rust,no_run
# use bevy_asset::AssetServer;
# use bevy_ecs::prelude::*;
# use bevy_state_tree::prelude::*;
# struct Simulation;
# enum SimulationEvents {}
# impl StateTreeEvents for SimulationEvents {
#     const AUTHORED_NAMES: &'static [&'static str] = &[];
#     fn event_index(&self) -> usize { match *self {} }
# }
# impl StateTreeDomain for Simulation { type Event = SimulationEvents; }
fn spawn_controller(mut commands: Commands, assets: Res<AssetServer>) {
    let definition = assets.load("state_trees/enemy.scxml");
    commands.spawn(StateTreeInstance::<Simulation>::new(definition));
}
```

Programmatically generated definitions use the same asset store:

```rust,no_run
# use bevy_asset::{Assets, Handle};
# use bevy_state_tree::prelude::*;
fn add_generated_definition(
    definitions: &mut Assets<StateTreeDefinition>,
) -> Result<Handle<StateTreeDefinition>, BuildError> {
    let mut builder = StateTreeBuilder::new("generated");
    let root = builder.compound_root("root")?;
    let idle = builder.atomic("idle", root)?;
    builder.initial(root, idle)?;
    Ok(definitions.add(builder.build()?))
}
```

The public builder is a low-level compilation target for programmatically
generated definitions. It represents every semantic construct accepted by the
SCXML compiler without providing alternate runtime semantics. Declarative
specifications preserve datamodel, transition, and action order while the
builder compiles them into the same dense runtime form:

```rust,no_run
# use bevy_state_tree::prelude::*;
# fn definition() -> Result<StateTreeDefinition, BuildError> {
let mut builder = StateTreeBuilder::new("counter");
builder.luau_datamodel([StateTreeData::new("count").with_initializer("0")])?;
let root = builder.compound_root("root")?;
let active = builder.atomic("active", root)?;
let done = builder.final_state("done", root)?;
builder.initial(root, active)?;
builder.entry_actions(active, [StateTreeAction::raise("ready")])?;
builder.add_transition(
    active,
    StateTreeTransition::on("tick")
        .condition("count < 10")
        .target(done)
        .actions([StateTreeAction::assign("count", "count + 1")]),
)?;
let definition = builder.build()?;
# Ok(definition)
# }
```

Static timeouts use `StateTreeTransition::after(Duration)` in generated
definitions or `bst:after` in SCXML. A timeout is armed whenever its source
state is entered, cancelled when that state exits, and rearmed from the current
clock on reentry. Targetless timeouts are one-shot. Timeouts that share a
deadline are selected as one transition cohort, while multiple overdue
deadlines are processed chronologically so an earlier transition can cancel a
later timeout.

Headless execution advances its explicit clock with
`StateTreeExecution::advance_time` and the corresponding traced or effect-sink
variants. Bevy execution reads `Time`, so `TimePlugin` must be installed before
`StateTreePlugin`. Due timeouts run in `StateTreeSet::ProcessTimers`, before
`QueueEvents` and `ProcessEvents`; a timeout therefore wins over an external
event delivered in the same schedule run when both leave the same active state.

Malformed XML and unsupported runtime semantics fail asset loading. Typed event
descriptors and effects are domain-specific, so they are linked later when an
instance initializes. A linking failure leaves the loaded definition intact and
adds `StateTreeLinkFailure<D>` to the pending instance.

Asset replacement is not currently applied to running instances. They retain
the definition against which they initialized; new and pending instances use
the replacement asset.

Definitions are linked against each domain's typed event protocol and effect
registry before initialization. An unmatched event descriptor or unregistered
effect leaves the instance pending and adds a queryable
`StateTreeLinkFailure<D>` component. Replacing the asset or registering the
missing effect retries linking without changing successfully running instances.

Entering a final child of the document root sets the instance status to
`Complete` and retains the root and reached final state for inspection. This is
not implicit teardown: the final state's exit actions do not run, and an ECS
system remains responsible for replacing or removing the completed controller.

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
Headless constructors return `Result` because the definition must still link to
the execution's typed event protocol.

## Semantic Tests

Each case named in `tests/scxml/manifest.txt` has a project-owned `.scxml`
source and `.trace` golden log. An optional `.events` sidecar supplies one
external authored event name per line from the harness's closed event protocol.
The corpus runner reports every mismatch together so adding cases requires no
case-specific Rust assertions.

## Benchmarks

Run `just bench` for both throughput and allocation profiles, or use
`just _bench-execution` and `just _bench-allocation` separately. Throughput cases
cover broadcast processing across as many as one million independent trees,
sparse entity events in a population of 100,000, initialization, deep re-entry,
transition scanning, typed effects, wide parallel configurations, far-future
idle timer populations, and dense timer expiry. Parsing, compilation, and
spawning remain outside steady-state processing timings; event cases queue and
process exactly one configured event per measured schedule run.

The allocation suite uses Divan's global allocator profiler and a single-threaded
Bevy schedule, so Bevy-owned worker threads cannot hide allocations.
Initialization cases measure the full pending-to-running lifecycle, while
spawn-and-initialize cases additionally include ECS component storage after
warming the empty schedule. Timer cases separately measure initialization and
arming plus steady-state dense expiry. Divan's `max alloc` column is peak net requested
memory attributed to the measured interval, not process RSS or total live heap;
setup-owned deallocations may reduce it in processing cases. Allocation
profiling affects timing, so compare its allocation columns rather than its
timings with the parallel throughput suite.
