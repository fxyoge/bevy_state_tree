# bevy_state_tree

Asset-backed hierarchical statecharts for Bevy.

The crate currently provides the runtime foundation:

- immutable `StateTreeDefinition` assets;
- entity-local, domain-typed `StateTreeInstance` components;
- atomic, compound, and final states;
- hierarchical and eventless external transitions;
- bounded iterative macrostep stabilization;
- direct integration with ordinary targeted and broadcast Bevy messages.

SCXML loading, activities, behavior-tree composites, guards, parallel regions,
history, and invocation are planned on top of this foundation.

```rust,no_run
use bevy_app::{App, FixedUpdate};
use bevy_asset::AssetPlugin;
use bevy_ecs::prelude::*;
use bevy_state_tree::prelude::*;

struct Simulation;

#[derive(Message, EntityEvent)]
struct Wake {
    entity: Entity,
}

let mut app = App::new();
app.add_plugins(AssetPlugin::default())
    .add_plugins(StateTreePlugin::<Simulation>::new(FixedUpdate))
    .register_targeted_state_tree_event::<Simulation, Wake>("wake");
```
