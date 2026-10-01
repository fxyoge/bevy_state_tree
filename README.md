# bevy_state_tree

Asset-backed hierarchical statecharts for Bevy.

The crate currently provides the runtime foundation:

- immutable `StateTreeDefinition` assets;
- entity-local, domain-typed `StateTreeInstance` components;
- atomic, compound, parallel, and final states;
- shallow history for compound and parallel states;
- hierarchical, eventless, and compound-completion transitions;
- external and internal transition semantics;
- static state-scoped `after` transitions with deterministic deadline ordering;
- Luau data expressions and transition guards;
- ordered SCXML `raise` and Luau `assign` actions on entry, exit, initial, and
  regular transitions;
- typed effects that construct and publish concrete Bevy messages;
- state-scoped typed activities with generation-safe completion and failure;
- opt-in typed event payload projection through Luau `_event.data`;
- bounded iterative macrostep stabilization;
- deterministic transition sets across orthogonal regions;
- ordered per-instance entity and broadcast event queues;
- parallel event processing across independent instances.

Behavior-tree composites and deep history remain future work. Standard SCXML
`<invoke>` is intentionally unsupported; long-running Bevy work uses the
structural `bst:activity` extension instead.

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

Project-owned SCXML fixtures under `tests/scxml` execute through a non-rendering
Bevy `App` and are compared with deterministic semantic event logs.

Each runtime domain implements `StateTreeDomain::configure` and declares its
complete event, effect, and activity protocol through `StateTreeConfig`.
`StateTreePlugin` consumes that declaration automatically when it is installed;
there is no application registration API or separate execution protocol.
Payloads are entity-targeted by default; add `broadcast` for domain-wide events.
Linking resolves authored names against the domain declaration and prelinks each
event type to dense transition sets. Processing does not scan strings or Rust
types for candidate transitions.

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

#[derive(EntityEvent, StateTreeEvent)]
#[state_tree(domain = Simulation, name = "wake", data = wake_data)]
struct Wake {
    entity: Entity,
}

fn wake_data(_: &Wake) -> Result<LuauValue, StateTreeEventDataError> {
    Ok(LuauValue::Record(
        [(Box::from("urgent"), LuauValue::Boolean(true))].into(),
    ))
}

impl StateTreeDomain for Simulation {
    fn configure(config: &mut StateTreeConfig<'_, Self>) {
        config.event::<Wake>();
    }
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
at the point where they were written. Private boxed ownership currently adds one
payload allocation per queued external event. A broadcast payload allocates once
and is shared across recipients. Storage remains opaque so measured workloads
can guide future optimization without changing the public event API.

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
# impl StateTreeDomain for Simulation {
#     fn configure(_config: &mut StateTreeConfig<'_, Self>) {}
# }
fn spawn_controller(mut commands: Commands, assets: Res<AssetServer>) {
    let definition = assets.load("state_trees/enemy.scxml");
    commands.spawn(StateTreeInstance::<Simulation>::new(definition));
}
```

Definitions can be shared while supplying instance-local initial datamodel
values. Authored initializers run first, then overrides are applied before any
initial state entry actions, guards, effects, or activities:

```rust,no_run
# use bevy_asset::Handle;
# use bevy_state_tree::prelude::*;
# struct Simulation;
# impl StateTreeDomain for Simulation {
#     fn configure(_config: &mut StateTreeConfig<'_, Self>) {}
# }
# let definition = Handle::<StateTreeDefinition>::default();
let initial_data = StateTreeDataOverrides::try_new([
    ("health", LuauValue::Integer(100)),
    ("speed", LuauValue::Number(4.5)),
])?;
let instance = StateTreeInstance::<Simulation>::new_with_initial_data(
    definition,
    initial_data,
);
# Ok::<(), StateTreeDataOverrideError>(())
```

Duplicate names are rejected when constructing the override set. A name absent
from the linked definition, including any override on a definition without a
Luau datamodel, faults that instance before it enters its initial states.

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

The public builder is a low-level, Rust-flavored compilation target for
programmatically generated definitions. It is intentionally not a convenience
DSL or a state-query API. It represents every semantic construct accepted by
the SCXML compiler without providing alternate runtime semantics. Declarative
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

Execution reads Bevy `Time`, so `TimePlugin` must be installed before
`StateTreePlugin`. Due timeouts run in `StateTreeSet::ProcessTimers`, before
`QueueEvents`, `ProcessActivityOutcomes`, and `ProcessEvents`; a timeout
therefore wins over an external event delivered in the same schedule run when
both leave the same active state.

Shallow history records an owner's immediately active children before the owner
exits. Restoring a compound child follows that child's normal initial
configuration; it does not restore deeper descendants. Timers and activities
restart through normal entry semantics rather than preserving elapsed time or
activity generations. A history target uses its required default child and
actions until its owner has exited once:

```rust,no_run
# use bevy_state_tree::prelude::*;
# fn definition() -> Result<StateTreeDefinition, BuildError> {
let mut builder = StateTreeBuilder::new("resume");
let root = builder.compound_root("root")?;
let movement = builder.compound("movement", root)?;
let idle = builder.atomic("idle", movement)?;
let running = builder.atomic("running", movement)?;
let paused = builder.atomic("paused", root)?;
let recent = builder.shallow_history("recent-movement", movement)?;
builder.history_default(recent, idle)?;
builder.initial(root, movement)?;
builder.initial(movement, idle)?;
builder.add_transition(running, StateTreeTransition::on("pause").target(paused))?;
builder.add_transition(paused, StateTreeTransition::on("resume").target(recent))?;
# builder.build()
# }
```

A compound state's initial target may itself be history, providing an
encapsulated "resume when re-entered" policy. Multiple shallow histories under
one owner may have different defaults but share the same stored configuration
after that owner exits.

Malformed XML and unsupported runtime semantics fail asset loading. Typed event
descriptors, effects, and activities are domain-specific, so they are linked
later when an instance initializes. A linking failure leaves the loaded
definition intact and adds `StateTreeLinkFailure<D>` to the pending instance.

Asset replacement is not currently applied to running instances. They retain
the definition against which they initialized; new and pending instances use
the replacement asset.

Definitions are linked against each domain's complete typed declaration before
initialization. An unmatched event descriptor or undeclared effect or activity
leaves the instance pending and adds a queryable `StateTreeLinkFailure<D>`
component. Replacing the asset retries linking without changing successfully
running instances; domain declarations are immutable after plugin installation.

For deterministic inspection, install `StateTreeTracePlugin::<D>::default()`
and read `StateTreeTraceLog<D>` from the App world. Logs are keyed by entity.
Without the trace plugin, macrosteps use the no-trace engine specialization: no
trace record is allocated, formatted, or locked.

The complete domain schedule order is `ProcessTimers`, `QueueEvents`,
`ProcessActivityOutcomes`, `Initialize`, `ProcessEvents`, `HandleActivities`,
then `HandleEffects`. Statechart actions can construct effect messages before
they generate activity lifecycle messages, but consumers always observe the
fixed `HandleActivities`-before-`HandleEffects` phase order.

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

Declare each authored name with `StateTreeConfig::effect` in the domain's
`configure` implementation. The constructor consumes named
`StateTreeEffectArgs` and returns one concrete
Bevy `Message`. Messages are available to systems in `StateTreeSet::HandleEffects`.
Construction follows statechart action order, but message delivery order between
parallel instances is unspecified. Constructor, argument, and payload projection
errors fault only the affected instance. A constructor can recover its concrete
trigger payload with `context.event::<Wake>()`; internal events and other
external payload types return `None`.

## Typed Activities

Activities are structural children of a `<state>` or `<parallel>`, not actions:

```xml
<state id="moving">
  <bst:activity id="path" name="find_path">
    <bst:arg name="destination" expr="target"/>
  </bst:activity>
  <transition event="done.activity.path" target="arrived"/>
  <transition event="error.activity.path" target="blocked"/>
</state>
```

The globally unique `id` and registered `name` are required. An activity `id`
must be exactly one event-name segment: non-empty ASCII letters, digits, `_`,
or `-` only. Arguments use the same Luau evaluation and named conversion rules
as effects. The constructor
returns `A`; systems in `HandleActivities` receive
`StateTreeActivityLifecycle<D, A>::Started { token, activity }` or
`Cancelled { token }`.

Activities start after owner entry actions and cancel after a committed exit.
Fault, root completion, and instance removal/replacement cancel every live run.
A terminal outcome leaves the owner active but deactivates the slot until
re-entry. `StateTreeActivityOutcomeWriter<D, A>` routes typed output/error values
using entity, runtime, activity, and generation tokens, so stale and duplicate
outcomes are ignored.

Output and error independently project portable Luau `_event.data`; projection
errors fault the target instance. The generated `done.activity.*` and
`error.activity.*` events are internal events: they remain inside the instance,
but can be handled with normal authored event descriptors. Activities are
declared with `StateTreeConfig::activity`; the crate does not prescribe futures
or tasks.

Every domain also includes the reserved `bst.spawn` activity. It takes one
required string argument, `definition`, loaded through `AssetServer`, and runs
that definition as a separate `StateTreeInstance<D>` entity:

```xml
<bst:activity id="combat" name="bst.spawn">
  <bst:arg name="definition" expr="'state_trees/combat.scxml'"/>
</bst:activity>
```

The child is logically owned by the activity but is not an ECS child. Cancelling
the activity despawns it. Child completion completes the activity; child faults,
link failures, load failures, and unexpected despawning fail it. Output is nil,
while failure `_event.data` is a diagnostic string.

## Global Bindings

A global binding is an activity-backed subscription to one value derived from
the Bevy world. Define the binding's value and event mapping:

```rust
#[derive(Clone)]
struct TimberBinding;

impl StateTreeBinding<UiDomain> for TimberBinding {
    type Value = u64;
    type Event = TimberChanged;

    fn event(&self, entity: Entity, amount: &u64) -> TimberChanged {
        TimberChanged {
            entity,
            amount: *amount,
        }
    }
}

fn timber_amount(domain: Res<ConstructionDomain>) -> u64 {
    domain.stockpile().timber()
}
```

Register the authored activity name and an ordinary no-input Bevy system that
returns the complete current value:

```rust
config.global_binding(
    "bind.timber",
    |_context, _arguments| Ok(TimberBinding),
    timber_amount,
);
```

`global_binding` registers its associated event automatically. While the
activity is active, the source system is sampled in `StateTreeSet::QueueEvents`.
Each new listener receives the current value once. Existing listeners receive
another event only when the returned value differs according to `PartialEq`.
Values may be application structs, enums, strings, collections, or other
semantic snapshots; the mechanism does not depend on Bevy change ticks.

Event declarations are idempotent, so explicitly declaring the associated event
before or after `global_binding` is also valid. A system that mutates the backing
data in `QueueEvents` can order itself before
`StateTreeBindingSet::<TimberBinding>::new()` when the new value must be visible
to the same schedule run's sample.

The source system is not run when there are no generation-valid listeners.
Activity exit, fault, completion, instance removal, and instance replacement
remove or invalidate listeners using the normal activity lifecycle. Since a
new activity starts after `QueueEvents`, its initial binding event is sampled
and delivered during the next run of the domain schedule.

Global bindings intentionally represent one sampled value shared by every
listener. Entity-dependent observations should use normal typed activities and
events until a separate entity-binding API establishes suitable query and
storage semantics.

## Semantic Tests

Each case named in `tests/scxml/manifest.txt` has a project-owned `.scxml`
source and `.trace` golden log. An optional `.events` sidecar supplies one
external authored event name per line from the harness's registered event
protocol. The corpus runner reports every mismatch together so adding cases
requires no case-specific Rust assertions.

## Benchmarks

Run `just bench` for the complete suite, or use `just _bench-execution` and
`just _bench-allocation` for the runtime profiles. `cargo bench --bench
event_storage` compares compact, padded, selectively boxed, and registered
erased queue representations across small and 256-byte payloads. Throughput
cases cover broadcast processing across as many as one million independent
trees, sparse entity events in a population of 100,000, initialization, activity
initialization and outcome processing, deep re-entry, transition scanning,
typed effects, shallow-history restoration, wide parallel configurations,
far-future idle timer populations,
and dense timer expiry. Parsing, compilation, and spawning remain outside
steady-state processing timings; event cases queue and process exactly one
configured event per measured schedule run.

The allocation suite uses Divan's global allocator profiler and a single-threaded
Bevy schedule, so Bevy-owned worker threads cannot hide allocations.
Initialization and activity initialization cases measure the full
pending-to-running lifecycle; activity outcome cases measure terminal outcome
processing. Spawn-and-initialize cases additionally include ECS component
storage after warming the empty schedule. Timer cases separately measure
initialization and arming plus steady-state dense expiry. Divan's `max alloc`
column is peak net requested memory attributed to the measured interval, not
process RSS or total live heap; setup-owned deallocations may reduce it in
processing cases. Allocation profiling affects timing, so compare its allocation
columns rather than its timings with the parallel throughput suite.
