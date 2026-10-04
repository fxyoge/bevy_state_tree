# bevy_state_tree

Asset-backed hierarchical statecharts for Bevy.

`bevy_state_tree` lets you author entity behavior as SCXML, load it through Bevy's asset system, and connect it to typed Bevy events, messages, and long-running activities. Definitions are compiled once, shared between entities, and executed independently in parallel.

Use it for game AI, UI controllers, animation graphs, and any other situation where hand-written state machines fail to express nested, concurrent, or data-driven behavior.

## What it provides

- Atomic, compound, parallel, final, and shallow-history states
- Event-driven, eventless, guarded, internal, completion, and timed transitions
- Luau expressions for data, guards, and assignments
- Typed Bevy events, effects, activities, and global bindings
- SCXML assets and a Rust builder for generated definitions

## Example

Author a state tree as an asset:

```xml
<scxml xmlns="http://www.w3.org/2005/07/scxml" initial="idle">
  <state id="idle">
    <transition event="enemy.spotted" target="chasing"/>
  </state>
  <state id="chasing">
    <transition event="enemy.lost" target="idle"/>
  </state>
</scxml>
```

Declare the typed events available to a domain:

```rust
use bevy_ecs::prelude::*;
use bevy_state_tree::prelude::*;

struct EnemyAi;

#[derive(EntityEvent, StateTreeEvent)]
#[state_tree(domain = EnemyAi, name = "enemy.spotted")]
struct EnemySpotted {
    entity: Entity,
}

#[derive(EntityEvent, StateTreeEvent)]
#[state_tree(domain = EnemyAi, name = "enemy.lost")]
struct EnemyLost {
    entity: Entity,
}

impl StateTreeDomain for EnemyAi {
    fn configure(config: &mut StateTreeConfig<'_, Self>) {
        config.event::<EnemySpotted>();
        config.event::<EnemyLost>();
    }
}
```

Install the plugin and attach the asset to an entity:

```rust
app.add_plugins(StateTreePlugin::<EnemyAi>::new(FixedUpdate));

let definition = asset_server.load("state_trees/enemy.scxml");
commands.spawn(StateTreeInstance::<EnemyAi>::new(definition));
```

State trees can emit typed messages for instantaneous effects and own typed activities for work that lives as long as a state, such as navigation, animation, or another state tree.

## Scope

This crate implements a Bevy-oriented subset of SCXML. See the [SCXML support policy](SCXML.md) for supported semantics and intentional differences.

`bevy_state_tree` currently targets Bevy 0.19 and is pre-1.0.
