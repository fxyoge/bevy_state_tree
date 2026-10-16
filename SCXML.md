# SCXML Support Policy

`bevy_state_tree` uses SCXML as an authored statechart format for Bevy. SCXML
compatibility is subordinate to a typed, data-oriented Bevy API: the project
does not aim to become a general-purpose SCXML interpreter.

The parser preserves the broader SCXML syntax so unsupported documents receive
useful diagnostics and future integrations do not require replacing the asset
format. The compiler accepts only behavior with production runtime semantics.
It rejects unsupported elements instead of silently ignoring them.

Every semantic construct accepted by the SCXML compiler is also available
through `StateTreeBuilder`. The builder is a low-level target for generated
definitions, not a separate state-machine DSL. Parity tests compile paired
SCXML and builder definitions and compare their complete runtime structure
after normalizing generated identities and source-only diagnostic offsets.

## Asset Loading

Installing `StateTreePlugin` registers `.scxml` as a Bevy asset source for
`StateTreeDefinition`. Each UTF-8 source is parsed and compiled once during
loading. Parser diagnostics retain the asset path, byte span, and line and
column; compiler diagnostics retain the asset path and byte position.

Asset loading is domain-independent. It validates authored SCXML and produces
the same immutable runtime representation as `StateTreeBuilder::build`, but it
does not resolve external events, typed effects, or typed activities. Those
checks depend on the domain declaration consumed by `StateTreePlugin` and occur
when a `StateTreeInstance` initializes. A source can load successfully and still
leave an instance pending with `StateTreeLinkFailure<D>` for a particular
domain.

Every SCXML document must declare its stable authored domain in the Bevy
state-tree namespace. The declaration is retained during domain-independent
asset loading and checked when an instance links:

```xml
<scxml xmlns="http://www.w3.org/2005/07/scxml"
       xmlns:bst="urn:bevy-state-tree"
       bst:domain="simulation">
```

The value must equal `StateTreeDomain::AUTHORED_NAME`. A mismatch produces a
single domain link issue before the domain's events, effects, and activities are
checked. Generated definitions may opt into the same check with
`StateTreeBuilder::domain`; generated definitions without one remain reusable
across domains. Each installed domain must have a unique authored name within
the Bevy application.

Definitions constructed programmatically can be inserted directly into
`Assets<StateTreeDefinition>`; they share the same initialization and linking
path as file-loaded definitions.

## Events

Each state-tree domain implements `StateTreeDomain::configure`. Every concrete
payload derives `StateTreeEvent` with its domain and authored name, then the
domain declares it with `StateTreeConfig::event`. Entity payloads also derive
`EntityEvent`; broadcast payloads derive Bevy `Event` and use the
`#[state_tree(..., broadcast)]` option.

`StateTreePlugin` consumes the complete declaration during installation. There
is no mutable application registration API and no separate portable protocol.
Each event in a domain must have a unique authored name, including entity and
broadcast events.

Authored descriptors are checked against registered authored names when a
definition is linked. Unlike general-purpose SCXML, descriptors name exact
events; prefix and wildcard matching are rejected so adding a typed event cannot
change existing statechart behavior. Linking precomputes dense transition sets
for each registered event, so macrosteps do not perform dynamic string matching.
Typed Rust payloads remain intact through queueing and execution, and typed
constructors can inspect the current payload with `context.event::<T>()`. The
runtime does not accept an unregistered arbitrary string as an external event.

An event payload may opt into Luau payload access with
`#[state_tree(name = "...", data = projector)]`. The projector accepts a shared
reference to the concrete payload and returns
`Result<LuauValue, StateTreeEventDataError>`. It runs lazily only when an
executed expression reads `_event.data`. Projection errors fault the affected
instance.

`<raise>` is supported because it creates a payload-free event on the
statechart's private internal queue. Its authored name is compiled to a dense
event identifier and does not escape as a Bevy event.

## Parallel States

`<parallel>` activates every direct region and recursively enters each region's
initial configuration. Regions are orthogonal, not concurrently executed: one
event selects a deterministic set of non-conflicting transitions and executes
the set serially. This keeps Luau assignments, internal raises, and typed
effects ordered even when multiple regions react to the same event.

Transitions in descendant states take priority over conflicting ancestor
transitions. Other conflicts are resolved in authored document order. For one
transition set, exits run deepest-first with document order as a tie breaker,
then transition actions run in document order, then entries run in document
order. A parallel state raises `done.state.<id>` after all of its regions have
completed. Multi-target transitions remain unsupported.

## Internal Transitions

`<transition type="internal">` follows SCXML transition-domain semantics. When
a compound source targets one of its descendants, the source remains active and
only its active descendants are exited. Internal transitions with any other
source-target relationship behave like external transitions. Transition actions
and descendant exit and entry actions retain their normal execution order.

## Shallow History

`<history>` with omitted `type` or `type="shallow"` is supported under compound
and parallel states. It requires exactly one transition whose single target is
an immediate real child of the owner. That transition cannot have an event,
condition, transition type, or `bst:after`, but it may contain the same supported
executable content as other transitions.

Immediately active children are recorded before any exit action whenever the
owner is exited. A compound owner records one child. A parallel owner records
all of its direct regions; restoring it restarts each region through its normal
initial descendants. Multiple shallow histories on one owner have independent
defaults and default actions but observe the same stored configuration.

When no configuration has been recorded, the default transition's content runs
after the owner's entry and initial-transition actions and before descendant
entry. If a transition to history leaves the owner active, those owner-entry and
default-transition actions do not run. Stored restoration suppresses the
default content in either case. History may be the target of an ordinary
transition or a compound state's initial transition. It is a pseudo-state and
never appears in the active configuration.

History stores configuration only. Datamodel values continue normally, while
timers and activities are cancelled on exit and start fresh on restored entry.
Deep history remains unsupported and is rejected explicitly.

## Completion

Entering a final child of a nested compound state raises `done.state.<id>` once
for that active configuration. A targetless handler does not leave and re-enter
the completed state, so it does not cause the completion event to repeat.

Entering a final child of the document root sets the instance status to
`Complete`. The synthetic document root and reached final state remain active
for inspection, and subsequent external events are ignored. Unlike SCXML
interpreter shutdown, completion does not execute the final state's exit
actions. Applications should put terminal output on final entry or the incoming
transition and let ECS lifecycle code replace or remove the completed instance.

## After Transitions

The Bevy extension namespace adds state-scoped after transitions with the
`bst:after` transition attribute:

```xml
<scxml xmlns="http://www.w3.org/2005/07/scxml"
       xmlns:bst="urn:bevy-state-tree">
  <state id="waiting">
    <transition bst:after="250ms" target="ready"/>
  </state>
  <state id="ready"/>
</scxml>
```

The value is a positive static duration: an integer or decimal number followed
immediately by `ms` or `s`, such as `250ms`, `1s`, or `1.5s`. Zero, signs,
exponents, non-finite values, overflow, whitespace, and other units are
rejected. `bst:after` cannot be combined with `event` on one transition.

An after transition is scoped to its source state. Its timeout starts when that
state is entered, is discarded when the state exits, and starts fresh on
re-entry. A pending timeout is considered before external events for the same
processing point. After transitions are not eventless transitions and therefore
do not participate in eventless stabilization merely because they omit
`event`.

## Send

SCXML `<send>` will not be supported. Its generic string event, dynamic
payload, target URI, I/O processor, and private timer semantics do not map
cleanly to Bevy's typed events, entities, schedules, and time resources. In
particular, `event="damage"` cannot construct an application-defined Rust value
such as `Damage { entity, amount }` without adding a second dynamic event system
that bypasses the domain's registered event protocol.

State trees receive external events through `EntityStateTreeEventWriter` and
`BroadcastStateTreeEventWriter`. Future state-tree outputs should likewise use
an explicit typed Bevy integration, rather than implementing SCXML `<send>` as
an arbitrary string queue.

## Datamodels

The compiler supports a root `datamodel="luau"` with early-bound
`<datamodel><data id="..." expr="..."/></datamodel>` declarations, transition
`cond` expressions, and `<assign location="..." expr="..."/>` executable
content. Initializers run in document order, and assignments preserve their
order relative to `<raise>` actions. Local datamodels, scripts, and the other
SCXML executable-content elements remain unsupported.

Applications may supply instance-local `StateTreeDataOverrides`. All authored
initializers run first, then overrides are applied before the initial state
configuration is entered. Duplicate override names are rejected when the set is
constructed. An override whose name is absent from the linked definition faults
only that instance before any state entry; no additional SCXML syntax is
introduced.

Each instance stores only portable nil, boolean, number, string, sequence, and
string-keyed record values. Functions, userdata, threads, sparse or mixed-key
tables, and shared or cyclic table graphs cannot be persisted. Instances never
retain handles into a Luau VM and may move between Bevy worker threads. Worker
threads reuse sandboxed VMs with bounded memory, expression interruption, and a
fresh authored environment for each evaluation; scripts cannot access Bevy's
`World`.

Luau compilation/evaluation and the portable `LuauValue` type are supplied by
the shared `bevy_luau` crate. Supported simple expressions use native read plans;
other expressions use cached bytecode with the same Luau semantics. The state
tree retains ownership of assignment ordering, persistence, event projection,
and authored diagnostics. Its evaluation policy preserves datamodel behavior;
KUI uses the shared engine's stricter read-only presentation policy.

`_event.name` exposes the currently processed typed event's authored name.
`_event.data` exposes the opt-in portable projection of the current external
event or accepted activity outcome. Activity data remains available through
eventless follow-up for that outcome, then is cleared before any raised or
completion internal event is consumed. It is absent for other internal events
and unprojected payloads. Other SCXML system variables are reserved but not yet
implemented. Datamodel errors fault
the instance rather than raising SCXML `error.execution` events. ECMAScript is
not a planned built-in datamodel.

## Typed Effects

The Bevy extension namespace `urn:bevy-state-tree` provides executable typed
effects:

```xml
<bst:effect name="deal_damage">
  <bst:arg name="amount" expr="base_damage * 2"/>
</bst:effect>
```

`<bst:effect>` requires exactly one non-empty `name` attribute and contains only
zero or more `<bst:arg name="..." expr="..."/>` children. Argument names must be
unique within the effect. Arguments require `datamodel="luau"`; an effect with
no arguments does not.

Applications register an authored effect name with a Rust constructor. The
constructor receives the entity, current external event, datamodel view, and
evaluated named arguments, and returns a concrete Bevy `Message`. The runtime
publishes that message through deferred commands. Systems ordered in
`StateTreeSet::HandleEffects` observe messages after state-tree processing.
The constructor can use `context.event::<T>()` to borrow a matching concrete
external payload; it returns `None` for internal events or another payload type.
Registered effect names are unique within a domain, while definitions may invoke
the same registered effect any number of times.

Effects are constructed in SCXML action order. Publication across independently
processed instances is unordered. A constructor, argument conversion, or
unregistered-effect error prevents linking; actions already run before a runtime
constructor or conversion error are not rolled back.

SCXML `<send>` remains unsupported and is not an alias for typed effects.

## Activities And Invoke

Standard SCXML `<invoke>` is explicitly unsupported. Its processor types,
sessions, autoforwarding, finalize content, and cancellation model are not
reinterpreted as Bevy work. The parser rejects `<invoke>` as unknown SCXML.

Long-running work uses a structural Bevy extension:

```xml
<bst:activity id="path" name="find_path">
  <bst:arg name="destination" expr="target"/>
</bst:activity>
```

`bst:activity` may occur directly under `<state>` or `<parallel>`. It is not
executable content and cannot occur in entry, exit, or transition actions. Its
globally unique `id` and registered `name` are required. The activity `id` must
be exactly one event-name segment: non-empty ASCII letters, digits, `_`, or `-`
only. Arguments follow effect rules: unique non-empty names, non-empty Luau
expressions, no content, and a
Luau datamodel when arguments are present. Builder definitions use
`StateTreeActivityDefinition` and `StateTreeBuilder::add_activity`.
Registered activity names are unique within a domain, while definitions may use
the same registered activity for multiple globally unique activity IDs.

Owner entry actions run before starts. A committed exit cancels live activities
after exit actions; fault, root completion, and component removal/replacement
cancel all live runs. Typed lifecycle messages carry an entity-routing token
containing runtime id, dense activity id, and generation.

Success and failure produce exact internal events `done.activity.<id>` and
`error.activity.<id>`. An accepted outcome deactivates the slot without exiting
its owner; only re-entry restarts it. Duplicate, stale-generation, and replaced
instance outcomes are ignored. Output and error independently project portable
Luau `_event.data`; projection failure faults the instance. Every accepted
outcome is a separate macrostep. Per-instance writer order is retained while
concurrent writer order is unspecified, like entity event ingress.

`bst.spawn` is a built-in activity name in every domain and cannot be replaced
by domain configuration. Its ordinary required `definition` string argument is
passed to `AssetServer::load<StateTreeDefinition>`. Once available, the asset is
run on a separate entity with logical activity ownership only (no `ChildOf`).
Cancellation is silent and despawns a running child. Completion produces nil
output; asset-load, runtime, link, and missing-child failures produce a string in
`_event.data`. This is an activity integration, not an alias for `<invoke>`.

## Semantic Traces

The opt-in `StateTreeTracePlugin<D>` records transition sets, state exit attempts
and commits, actions, entries, configurations, and status changes in
deterministic semantic order. `StateTreeTraceLog<D>` exposes per-entity event
logs. Tracing does not expose task scheduling or speculative transition
selection, and the normal App path performs no per-record work when the plugin
is absent. Project-owned SCXML fixtures and checked-in `.trace` files exercise
the parser, compiler, and non-rendering Bevy App runtime as one black box.
