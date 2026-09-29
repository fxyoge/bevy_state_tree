# SCXML Support Policy

`bevy_state_tree` uses SCXML as an authored statechart format for Bevy. SCXML
compatibility is subordinate to a typed, data-oriented Bevy API: the project
does not aim to become a general-purpose SCXML interpreter.

The parser preserves the broader SCXML syntax so unsupported documents receive
useful diagnostics and future integrations do not require replacing the asset
format. The compiler accepts only behavior with production runtime semantics.
It rejects unsupported elements instead of silently ignoring them.

## Asset Loading

Installing `StateTreePlugin` registers `.scxml` as a Bevy asset source for
`StateTreeDefinition`. Each UTF-8 source is parsed and compiled once during
loading. Parser diagnostics retain the asset path, byte span, and line and
column; compiler diagnostics retain the asset path and byte position.

Asset loading is domain-independent. It validates authored SCXML and produces
the same immutable runtime representation as `StateTreeBuilder::build`, but it
does not resolve external events or typed effects. Those checks depend on a
domain's `StateTreeEvents` protocol and effect registry and therefore occur when
a `StateTreeInstance` initializes. A source can load successfully and still
leave an instance pending with `StateTreeLinkFailure<D>` for a particular
domain.

Definitions constructed programmatically can be inserted directly into
`Assets<StateTreeDefinition>`; they share the same initialization and linking
path as file-loaded definitions.

## Events

External events belong to a closed Rust protocol declared with
`#[derive(StateTreeEvents)]`. Authored event descriptors are resolved against
that protocol when a definition is linked to a state-tree domain. Typed Rust
payloads remain intact through queueing and execution; the runtime does not
dispatch arbitrary string events.

An event variant may opt into Luau payload access with
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

## Send

SCXML `<send>` will not be supported. Its generic string event, dynamic
payload, target URI, I/O processor, and private timer semantics do not map
cleanly to Bevy's typed events, entities, schedules, and time resources. In
particular, `event="damage"` cannot construct an application-defined Rust value
such as `Damage { entity, amount }` without adding a second dynamic event system
that bypasses the domain's `StateTreeEvents` protocol.

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

Each instance stores only portable nil, boolean, number, string, sequence, and
string-keyed record values. Functions, userdata, threads, sparse or mixed-key
tables, and shared or cyclic table graphs cannot be persisted. Instances never
retain handles into a Luau VM and may move between Bevy worker threads. Worker
threads reuse sandboxed VMs with bounded memory, expression interruption, and a
fresh authored environment for each evaluation; scripts cannot access Bevy's
`World`.

`_event.name` exposes the currently processed typed event's authored name.
`_event.data` exposes the opt-in portable projection of the current external
event. It is absent for internal events and unprojected variants. Other SCXML
system variables are reserved but not yet implemented. Datamodel errors fault
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

Effects are constructed in SCXML action order. Publication across independently
processed instances is unordered. A constructor, argument conversion, or
unregistered-effect error faults the affected instance; actions already run in
that macrostep are not rolled back. Headless execution can provide a
`StateTreeEffectSink`; without one, encountering an effect faults the execution.

SCXML `<send>` remains unsupported and is not an alias for typed effects.

## Semantic Traces

The public opt-in trace API records transition sets, state exit attempts and
commits, actions, entries, configurations, and status changes in deterministic
semantic order. It does not expose task scheduling or speculative transition
selection. Project-owned SCXML fixtures and checked-in `.trace` files exercise
the parser, compiler, and headless runtime as one black box.
