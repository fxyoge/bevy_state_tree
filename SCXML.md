# SCXML Support Policy

`bevy_state_tree` uses SCXML as an authored statechart format for Bevy. SCXML
compatibility is subordinate to a typed, data-oriented Bevy API: the project
does not aim to become a general-purpose SCXML interpreter.

The parser preserves the broader SCXML syntax so unsupported documents receive
useful diagnostics and future integrations do not require replacing the asset
format. The compiler accepts only behavior with production runtime semantics.
It rejects unsupported elements instead of silently ignoring them.

## Events

External events belong to a closed Rust protocol declared with
`#[derive(StateTreeEvents)]`. Authored event descriptors are resolved against
that protocol when a definition is linked to a state-tree domain. Typed Rust
payloads remain intact through queueing and execution; the runtime does not
dispatch arbitrary string events.

`<raise>` is supported because it creates a payload-free event on the
statechart's private internal queue. Its authored name is compiled to a dense
event identifier and does not escape as a Bevy event.

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
Other SCXML system variables are reserved but not yet implemented. Datamodel
errors fault the instance rather than raising SCXML `error.execution` events.
ECMAScript is not a planned built-in datamodel.

## Semantic Traces

The public opt-in trace API records transition sets, state exit attempts and
commits, actions, entries, configurations, and status changes in deterministic
semantic order. It does not expose task scheduling or speculative transition
selection. Project-owned SCXML fixtures and checked-in `.trace` files exercise
the parser, compiler, and headless runtime as one black box.
