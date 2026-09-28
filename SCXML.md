# SCXML Support Policy

`bevy_state_tree` uses SCXML as an authored statechart format for Bevy. SCXML
compatibility is subordinate to a typed, data-oriented Bevy API: the project
does not aim to become a general-purpose SCXML interpreter or to maximize its
W3C implementation-report pass count.

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

ECMAScript is not a planned built-in datamodel. Guards, actions, and data access
should integrate with typed Rust and Bevy first. Optional language integrations
such as Rhai or Lua may be considered without making a scripting engine part of
the core runtime.

## W3C Suite

The vendored W3C suite is a semantic regression corpus, not the product
roadmap. `tests/w3c/expected.toml` records both passing resources and explicit
reasons for expected failures, including deliberate differences from general
SCXML processors.
