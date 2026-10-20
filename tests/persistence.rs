#![cfg(feature = "persistence")]

#[derive(Component, Serialize, Deserialize)]
struct Attached(u32);
#[derive(Component, Serialize, Deserialize)]
struct ReferenceCollection {
    #[serde(with = "bevy_state_tree::persistence::entities")]
    members: Vec<Entity>,
    #[serde(with = "bevy_state_tree::persistence::entity_map")]
    edges: bevy_ecs::entity::EntityHashMap<ReferenceTarget>,
    #[serde(with = "bevy_state_tree::persistence::asset")]
    definition: bevy_asset::Handle<StateTreeDefinition>,
}
#[derive(Serialize, Deserialize)]
struct ReferenceTarget(#[serde(with = "bevy_state_tree::persistence::entity")] Entity);

#[test]
fn attached_components_do_not_select_platform_entities_and_nested_references_remap() {
    let mut source = app();
    source
        .persist_component::<ReferenceCollection>("refs")
        .persist_attached_component::<Attached>("attached");
    let root = install(&mut source, definition());
    let handle = source
        .world()
        .resource::<bevy_asset::AssetServer>()
        .load("unloaded-reference.scxml");
    source.world_mut().entity_mut(root).insert((
        Attached(17),
        ReferenceCollection {
            members: vec![root],
            edges: [(root, ReferenceTarget(root))].into(),
            definition: handle,
        },
    ));
    source.world_mut().spawn(Attached(99));
    let checkpoint = Checkpoint::capture(source.world_mut()).unwrap();
    let mut target = app();
    target
        .persist_component::<ReferenceCollection>("refs")
        .persist_attached_component::<Attached>("attached");
    let platform = target.world_mut().spawn(Attached(88)).id();
    install(&mut target, definition());
    checkpoint.restore(target.world_mut()).unwrap();
    let (entity, refs, attached) = target
        .world_mut()
        .query::<(Entity, &ReferenceCollection, &Attached)>()
        .single(target.world())
        .unwrap();
    assert_eq!(attached.0, 17);
    assert_eq!(refs.members, [entity]);
    assert_eq!(refs.edges[&entity].0, entity);
    assert_eq!(
        refs.definition.path().unwrap().to_string(),
        "unloaded-reference.scxml"
    );
    assert_eq!(target.world().get::<Attached>(platform).unwrap().0, 88);
    assert_eq!(
        target
            .world_mut()
            .query::<&Attached>()
            .iter(target.world())
            .count(),
        2
    );
}

use bevy_app::{App, TaskPoolPlugin, Update};
use bevy_asset::{AssetPlugin, Assets};
use bevy_ecs::{prelude::*, system::RunSystemOnce};
use bevy_state_tree::{
    persistence::{AppPersistenceExt, Checkpoint},
    prelude::*,
};
use bevy_time::{Time, TimePlugin};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, time::Duration};

struct Simulation;
impl StateTreeDomain for Simulation {
    const AUTHORED_NAME: &'static str = "save-test";
    fn configure(config: &mut StateTreeConfig<'_, Self>) {
        config
            .event::<Kick>()
            .event::<Mark>()
            .activity("work", |_, _| Ok(Work))
            .effect("grant", |context, _| {
                Ok(Grant(
                    context.event::<Kick>().map_or(1, |event| event.amount),
                ))
            });
    }
}

// Neither requests nor domains need serialization implementations. The actual
// running state is ordinary game data, registered once below.
struct Work;
impl StateTreeActivity for Work {
    type Output = ();
    type Error = ();
}
type Token = StateTreeActivityToken<Simulation, Work>;

#[derive(EntityEvent, StateTreeEvent)]
#[state_tree(domain = Simulation, name = "kick", data = kick_data)]
struct Kick {
    entity: Entity,
    amount: u32,
}

fn kick_data(event: &Kick) -> Result<LuauValue, StateTreeEventDataError> {
    Ok(LuauValue::Integer(event.amount.into()))
}

#[derive(Clone, Copy, Event, StateTreeEvent)]
#[state_tree(domain = Simulation, name = "mark", broadcast)]
enum Mark {
    A,
    B,
}

#[derive(Message)]
struct Grant(u32);

#[derive(Resource, Default)]
struct Observations {
    starts: usize,
    grants: u32,
}

#[derive(Serialize, Deserialize)]
struct Progress {
    work: u32,
}

#[derive(Resource, Default, Serialize, Deserialize)]
struct Runs {
    progress: HashMap<Token, Progress>,
    nested: Vec<Option<Token>>,
}

#[derive(Component, Serialize, Deserialize)]
struct SavedOwner {
    #[serde(with = "bevy_state_tree::persistence::entity")]
    peer: Entity,
}

fn lifecycle(
    mut messages: MessageReader<StateTreeActivityLifecycle<Simulation, Work>>,
    mut runs: ResMut<Runs>,
    mut observations: ResMut<Observations>,
) {
    for message in messages.read() {
        match message {
            StateTreeActivityLifecycle::Started { token, .. } => {
                observations.starts += 1;
                runs.progress.insert(*token, Progress { work: 0 });
                runs.nested.push(Some(*token));
            }
            StateTreeActivityLifecycle::Cancelled { token } => {
                runs.progress.remove(token);
                runs.nested.retain(|value| value.as_ref() != Some(token));
            }
        }
    }
}

fn effects(mut messages: MessageReader<Grant>, mut observations: ResMut<Observations>) {
    for Grant(amount) in messages.read() {
        observations.grants += amount;
    }
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((
        TaskPoolPlugin::default(),
        TimePlugin,
        AssetPlugin::default(),
        StateTreePlugin::<Simulation>::new(Update),
    ))
    .init_resource::<Runs>()
    .init_resource::<Observations>()
    .persist_resource::<Runs>("runs")
    .persist_component::<SavedOwner>("owners")
    .add_systems(Update, lifecycle.in_set(StateTreeSet::HandleActivities))
    .add_systems(Update, effects.in_set(StateTreeSet::HandleEffects));
    app
}

fn definition() -> StateTreeDefinition {
    let mut builder = StateTreeBuilder::new("persistent-worker");
    builder
        .luau_datamodel([
            StateTreeData::new("entries").with_initializer("0"),
            StateTreeData::new("completions").with_initializer("0"),
            StateTreeData::new("log").with_initializer("''"),
        ])
        .unwrap();
    let root = builder.compound_root("root").unwrap();
    let active = builder.atomic("active", root).unwrap();
    let done = builder.final_state("done", root).unwrap();
    builder.initial(root, active).unwrap();
    builder
        .entry_actions(
            active,
            [
                StateTreeAction::assign("entries", "entries + 1"),
                StateTreeAction::effect("grant", []),
            ],
        )
        .unwrap();
    builder
        .add_activity(active, StateTreeActivityDefinition::new("job", "work"))
        .unwrap();
    builder
        .add_transition(
            active,
            StateTreeTransition::after(Duration::from_secs(10))
                .timer_id("finish")
                .target(done),
        )
        .unwrap();
    builder
        .add_transition(
            active,
            StateTreeTransition::on("done.activity.job")
                .actions([StateTreeAction::assign("completions", "completions + 1")]),
        )
        .unwrap();
    builder
        .add_transition(
            active,
            StateTreeTransition::on("kick").actions([
                StateTreeAction::assign("log", "log .. tostring(_event.data)"),
                StateTreeAction::effect("grant", []),
            ]),
        )
        .unwrap();
    builder
        .add_transition(
            active,
            StateTreeTransition::on("mark").actions([StateTreeAction::assign("log", "log .. 'm'")]),
        )
        .unwrap();
    builder.build().unwrap()
}

fn install(app: &mut App, definition: StateTreeDefinition) -> Entity {
    let handle = app
        .world_mut()
        .resource_mut::<Assets<StateTreeDefinition>>()
        .add(definition);
    let entity = app
        .world_mut()
        .spawn(StateTreeInstance::<Simulation>::new(handle))
        .id();
    app.world_mut()
        .entity_mut(entity)
        .insert(SavedOwner { peer: entity });
    app.world_mut().run_schedule(Update);
    entity
}

fn owner(app: &mut App) -> Entity {
    app.world_mut()
        .query_filtered::<Entity, With<SavedOwner>>()
        .single(app.world())
        .unwrap()
}

fn token(app: &App) -> Token {
    *app.world()
        .resource::<Runs>()
        .progress
        .keys()
        .next()
        .unwrap()
}

fn complete(app: &mut App, token: Token) -> bool {
    app.world_mut()
        .run_system_once(
            move |mut writer: StateTreeActivityOutcomeWriter<Simulation, Work>| {
                writer.complete(token, ())
            },
        )
        .unwrap()
}

fn kick(app: &mut App, entity: Entity, amount: u32) {
    app.world_mut()
        .run_system_once(move |mut writer: EntityStateTreeEventWriter<Simulation>| {
            writer.write(Kick { entity, amount });
        })
        .unwrap();
}

fn mark(app: &mut App, value: Mark) {
    app.world_mut()
        .run_system_once(
            move |mut writer: BroadcastStateTreeEventWriter<Simulation>| {
                writer.write(value);
            },
        )
        .unwrap();
}

fn data(app: &App, entity: Entity, key: &str) -> LuauValue {
    app.world()
        .get::<StateTreeInstance<Simulation>>(entity)
        .unwrap()
        .data(key)
        .unwrap()
        .clone()
}

#[test]
fn progress_nested_tokens_and_map_keys_restore_without_consumer_adapters_or_replayed_effects() {
    let mut app = app();
    let old_owner = install(&mut app, definition());
    let old_token = token(&app);
    app.world_mut()
        .resource_mut::<Runs>()
        .progress
        .get_mut(&old_token)
        .unwrap()
        .work = 73;
    app.world_mut()
        .resource_mut::<Time>()
        .advance_by(Duration::from_secs(3));
    app.world_mut().run_schedule(Update);
    let checkpoint = Checkpoint::capture(app.world_mut()).unwrap();
    let bytes = serde_json::to_vec(&checkpoint).unwrap();
    let checkpoint: Checkpoint = serde_json::from_slice(&bytes).unwrap();
    app.world_mut()
        .resource_mut::<Time>()
        .advance_by(Duration::from_secs(97));
    checkpoint.restore(app.world_mut()).unwrap();
    let owner = owner(&mut app);
    let restored_token = token(&app);
    assert_ne!(owner, old_owner);
    assert_ne!(old_token, restored_token);
    assert_eq!(restored_token.entity(), owner);
    assert_eq!(app.world().get::<SavedOwner>(owner).unwrap().peer, owner);
    let runs = app.world().resource::<Runs>();
    assert_eq!(runs.progress[&restored_token].work, 73);
    assert_eq!(runs.nested, [Some(restored_token)]);
    assert!(!complete(&mut app, old_token));
    app.world_mut().run_schedule(Update);
    assert_eq!(app.world().resource::<Observations>().starts, 1);
    assert_eq!(app.world().resource::<Observations>().grants, 1);
    assert_eq!(data(&app, owner, "entries"), LuauValue::Integer(1));
    assert_eq!(
        app.world()
            .get::<StateTreeInstance<Simulation>>(owner)
            .unwrap()
            .status(),
        &InstanceStatus::Running
    );
    app.world_mut()
        .resource_mut::<Time>()
        .advance_by(Duration::from_secs(6));
    app.world_mut().run_schedule(Update);
    assert_eq!(
        app.world()
            .get::<StateTreeInstance<Simulation>>(owner)
            .unwrap()
            .status(),
        &InstanceStatus::Running
    );
    app.world_mut()
        .resource_mut::<Time>()
        .advance_by(Duration::from_secs(1));
    app.world_mut().run_schedule(Update);
    assert_eq!(
        app.world()
            .get::<StateTreeInstance<Simulation>>(owner)
            .unwrap()
            .status(),
        &InstanceStatus::Complete
    );
}

#[test]
fn checkpoint_loads_in_a_fresh_world_with_recompiled_definition_and_repeated_loads_reject_old_tokens()
 {
    let mut source = app();
    install(&mut source, definition());
    let source_token = token(&source);
    source
        .world_mut()
        .resource_mut::<Runs>()
        .progress
        .get_mut(&source_token)
        .unwrap()
        .work = 42;
    let checkpoint = Checkpoint::capture(source.world_mut()).unwrap();
    let mut target = app();
    // Independent compilation allocates new definition/state identities.
    target
        .world_mut()
        .resource_mut::<Assets<StateTreeDefinition>>()
        .add(definition());
    checkpoint.restore(target.world_mut()).unwrap();
    let first = token(&target);
    assert_eq!(target.world().resource::<Runs>().progress[&first].work, 42);
    target.world_mut().run_schedule(Update);
    assert_eq!(target.world().resource::<Observations>().starts, 0);
    assert_eq!(target.world().resource::<Observations>().grants, 0);
    checkpoint.restore(target.world_mut()).unwrap();
    assert!(!complete(&mut target, first));
    let second = token(&target);
    assert!(complete(&mut target, second));
}

#[test]
fn pending_typed_events_broadcast_order_and_outcomes_round_trip() {
    let mut app = app();
    let old_owner = install(&mut app, definition());
    let run = token(&app);
    kick(&mut app, old_owner, 2);
    mark(&mut app, Mark::A);
    kick(&mut app, old_owner, 3);
    mark(&mut app, Mark::B);
    assert!(complete(&mut app, run));
    assert!(!complete(&mut app, run));
    let checkpoint = Checkpoint::capture(app.world_mut()).unwrap();
    checkpoint.restore(app.world_mut()).unwrap();
    let owner = owner(&mut app);
    app.world_mut().run_schedule(Update);
    assert_eq!(data(&app, owner, "log"), LuauValue::String("2m3m".into()));
    assert_eq!(data(&app, owner, "completions"), LuauValue::Integer(1));
    assert_eq!(app.world().resource::<Observations>().grants, 6);
    assert_eq!(app.world().resource::<Observations>().starts, 1);
    // Completed activity stays inactive even while its owning state stays active.
    // A stale reference must never silently turn into a live run on reload.
    assert!(Checkpoint::capture(app.world_mut()).is_err());
    // Consumers should remove terminal run records as part of normal lifecycle;
    // remove this test's deliberately retained references before capturing again.
    app.world_mut().resource_mut::<Runs>().progress.clear();
    app.world_mut().resource_mut::<Runs>().nested.clear();
    let completed = Checkpoint::capture(app.world_mut()).unwrap();
    completed.restore(app.world_mut()).unwrap();
    app.world_mut().run_schedule(Update);
    assert_eq!(app.world().resource::<Observations>().starts, 1);
}

#[test]
fn malformed_data_and_changed_definitions_do_not_replace_existing_state() {
    let mut app = app();
    let old_owner = install(&mut app, definition());
    let old_token = token(&app);
    let checkpoint = Checkpoint::capture(app.world_mut()).unwrap();
    let entity_count = app.world_mut().query::<Entity>().iter(app.world()).count();
    let mut wire = serde_json::to_value(&checkpoint).unwrap();
    wire["data"]["runs"][0][1] = serde_json::json!([123]);
    let corrupt: Checkpoint = serde_json::from_value(wire).unwrap();
    assert!(corrupt.restore(app.world_mut()).is_err());
    assert_eq!(owner(&mut app), old_owner);
    assert_eq!(token(&app), old_token);
    assert_eq!(
        app.world_mut().query::<Entity>().iter(app.world()).count(),
        entity_count
    );
    let handle = app
        .world()
        .get::<StateTreeInstance<Simulation>>(old_owner)
        .unwrap()
        .definition()
        .clone();
    let mut different = StateTreeBuilder::new("persistent-worker");
    let root = different.compound_root("root").unwrap();
    let other = different.atomic("other", root).unwrap();
    different.initial(root, other).unwrap();
    app.world_mut()
        .resource_mut::<Assets<StateTreeDefinition>>()
        .insert(handle.id(), different.build().unwrap())
        .unwrap();
    assert!(checkpoint.restore(app.world_mut()).is_err());
    assert_eq!(owner(&mut app), old_owner);
    assert_eq!(token(&app), old_token);
}

#[test]
fn token_serialization_outside_a_checkpoint_fails_instead_of_writing_runtime_ids() {
    let mut app = app();
    install(&mut app, definition());
    assert!(serde_json::to_vec(&token(&app)).is_err());
    assert!(serde_json::from_str::<Token>("0").is_err());
    let checkpoint = Checkpoint::capture(app.world_mut()).unwrap();
    checkpoint.restore(app.world_mut()).unwrap();
    assert!(serde_json::to_vec(&token(&app)).is_err());
}

fn xml_definition(name: &str, body: &str) -> StateTreeDefinition {
    bevy_state_tree::scxml::compile(
        &bevy_state_tree::scxml::parse(
            name,
            &format!(
                r#"
        <scxml xmlns="http://www.w3.org/2005/07/scxml" xmlns:bst="urn:bevy-state-tree"
            bst:domain="save-test" datamodel="luau">{body}</scxml>"#
            ),
        )
        .unwrap(),
    )
    .unwrap()
}

#[test]
fn shallow_history_restores_prior_child_instead_of_reentering_initial_child() {
    let definition = xml_definition(
        "saved-history",
        r#"
        <state id="inside" initial="first">
          <history id="recent"><transition target="first"/></history>
          <state id="first"><transition event="kick" target="second"/></state>
          <state id="second"><transition event="kick" target="outside"/></state>
        </state>
        <state id="outside"><transition event="kick" target="recent"/></state>"#,
    );
    let mut app = app();
    let original = install(&mut app, definition);
    kick(&mut app, original, 1);
    app.world_mut().run_schedule(Update);
    kick(&mut app, original, 1);
    app.world_mut().run_schedule(Update);
    let checkpoint = Checkpoint::capture(app.world_mut()).unwrap();
    checkpoint.restore(app.world_mut()).unwrap();
    let restored = owner(&mut app);
    kick(&mut app, restored, 1);
    app.world_mut().run_schedule(Update);
    assert_eq!(
        app.world()
            .get::<StateTreeInstance<Simulation>>(restored)
            .unwrap()
            .active_state_names()
            .last(),
        Some("second")
    );
}

#[test]
fn parallel_completion_bookkeeping_does_not_emit_a_second_completion_after_restore() {
    let definition = xml_definition(
        "saved-parallel",
        r#"
        <datamodel><data id="completions" expr="0"/></datamodel>
        <parallel id="both">
          <transition event="done.state.left"><assign location="completions" expr="completions + 1"/></transition>
          <state id="left" initial="left-start">
            <state id="left-start"><transition event="kick" cond="_event.data == 1" target="left-done"/></state>
            <final id="left-done"/>
          </state>
          <state id="right" initial="right-a">
            <state id="right-a"><transition event="kick" cond="_event.data == 2" target="right-b"/></state>
            <state id="right-b"><transition event="kick" cond="_event.data == 2" target="right-a"/></state>
          </state>
        </parallel>"#,
    );
    let mut app = app();
    let original = install(&mut app, definition);
    kick(&mut app, original, 1);
    app.world_mut().run_schedule(Update);
    assert_eq!(data(&app, original, "completions"), LuauValue::Integer(1));
    let checkpoint = Checkpoint::capture(app.world_mut()).unwrap();
    checkpoint.restore(app.world_mut()).unwrap();
    let restored = owner(&mut app);
    kick(&mut app, restored, 2);
    app.world_mut().run_schedule(Update);
    assert_eq!(data(&app, restored, "completions"), LuauValue::Integer(1));
    assert!(
        app.world()
            .get::<StateTreeInstance<Simulation>>(restored)
            .unwrap()
            .active_state_names()
            .any(|name| name == "right-b")
    );
}

#[test]
fn disabled_trees_are_included_and_remain_disabled() {
    use bevy_ecs::entity_disabling::Disabled;
    let mut app = app();
    let original = install(&mut app, definition());
    app.world_mut().entity_mut(original).insert(Disabled);
    let checkpoint = Checkpoint::capture(app.world_mut()).unwrap();
    checkpoint.restore(app.world_mut()).unwrap();
    let restored = token(&app).entity();
    assert_ne!(restored, original);
    assert!(app.world().get::<Disabled>(restored).is_some());
    assert!(app.world().get_entity(original).is_err());
    assert_eq!(
        app.world().get::<SavedOwner>(restored).unwrap().peer,
        restored
    );
}

#[test]
fn faulted_instances_and_nonfinite_luau_values_round_trip_without_vm_handles() {
    let mut app = app();
    let mut builder = StateTreeBuilder::new("faulted-numbers");
    builder
        .luau_datamodel([
            StateTreeData::new("number"),
            StateTreeData::new("negative_zero"),
        ])
        .unwrap();
    let root = builder.compound_root("root").unwrap();
    let active = builder.atomic("active", root).unwrap();
    builder.initial(root, active).unwrap();
    builder
        .add_transition(active, StateTreeTransition::eventless().target(active))
        .unwrap();
    let handle = app
        .world_mut()
        .resource_mut::<Assets<StateTreeDefinition>>()
        .add(builder.build().unwrap());
    let nan = f64::from_bits(0x7ff8_0000_0000_0123);
    let overrides = StateTreeDataOverrides::try_new([
        ("number", LuauValue::Number(nan)),
        ("negative_zero", LuauValue::Number(-0.0)),
    ])
    .unwrap();
    let entity = app
        .world_mut()
        .spawn(StateTreeInstance::<Simulation>::new_with_initial_data(
            handle, overrides,
        ))
        .id();
    app.world_mut()
        .entity_mut(entity)
        .insert(SavedOwner { peer: entity });
    app.world_mut().run_schedule(Update);
    assert!(matches!(
        app.world()
            .get::<StateTreeInstance<Simulation>>(entity)
            .unwrap()
            .status(),
        InstanceStatus::Faulted(_)
    ));
    let checkpoint = Checkpoint::capture(app.world_mut()).unwrap();
    let bytes = serde_json::to_vec(&checkpoint).unwrap();
    serde_json::from_slice::<Checkpoint>(&bytes)
        .unwrap()
        .restore(app.world_mut())
        .unwrap();
    let restored = owner(&mut app);
    let LuauValue::Number(value) = data(&app, restored, "number") else {
        panic!("number type changed")
    };
    assert_eq!(value.to_bits(), nan.to_bits());
    let LuauValue::Number(value) = data(&app, restored, "negative_zero") else {
        panic!("number type changed")
    };
    assert_eq!(value.to_bits(), (-0.0f64).to_bits());
    app.world_mut().run_schedule(Update);
    assert!(matches!(
        app.world()
            .get::<StateTreeInstance<Simulation>>(restored)
            .unwrap()
            .status(),
        InstanceStatus::Faulted(_)
    ));
}

#[test]
fn pending_instances_keep_initial_overrides_and_initialize_exactly_once_after_restore() {
    let mut app = app();
    let handle = app
        .world_mut()
        .resource_mut::<Assets<StateTreeDefinition>>()
        .add(definition());
    let initial = StateTreeDataOverrides::try_new([("entries", LuauValue::Integer(5))]).unwrap();
    let original = app
        .world_mut()
        .spawn(StateTreeInstance::<Simulation>::new_with_initial_data(
            handle, initial,
        ))
        .id();
    app.world_mut()
        .entity_mut(original)
        .insert(SavedOwner { peer: original });
    let checkpoint = Checkpoint::capture(app.world_mut()).unwrap();
    checkpoint.restore(app.world_mut()).unwrap();
    let restored = owner(&mut app);
    assert_eq!(
        app.world()
            .get::<StateTreeInstance<Simulation>>(restored)
            .unwrap()
            .status(),
        &InstanceStatus::Pending
    );
    app.world_mut().run_schedule(Update);
    app.world_mut().run_schedule(Update);
    assert_eq!(data(&app, restored, "entries"), LuauValue::Integer(6));
    assert_eq!(app.world().resource::<Observations>().starts, 1);
    assert_eq!(app.world().resource::<Observations>().grants, 1);
}

#[test]
fn capture_rejects_an_overdue_clock_boundary_instead_of_merging_timer_cohorts() {
    let mut app = app();
    install(&mut app, definition());
    app.world_mut()
        .resource_mut::<Time>()
        .advance_by(Duration::from_secs(11));
    assert!(Checkpoint::capture(app.world_mut()).is_err());
    app.world_mut().run_schedule(Update);
    app.world_mut().resource_mut::<Runs>().nested.clear();
    assert!(Checkpoint::capture(app.world_mut()).is_ok());
}

struct Other;
impl StateTreeDomain for Other {
    const AUTHORED_NAME: &'static str = "other-save-test";
    fn configure(config: &mut StateTreeConfig<'_, Self>) {
        config.activity("work", |_, _| Ok(OtherWork));
    }
}
struct OtherWork;
impl StateTreeActivity for OtherWork {
    type Output = ();
    type Error = ();
}
#[derive(Resource, Default, Serialize, Deserialize)]
struct OtherRun(Option<StateTreeActivityToken<Other, OtherWork>>);

#[test]
fn domains_sharing_an_owner_restore_distinct_typed_runs_and_reject_cross_domain_references() {
    let mut app = app();
    app.add_plugins(StateTreePlugin::<Other>::new(Update))
        .init_resource::<OtherRun>()
        .persist_resource::<OtherRun>("other-run")
        .add_systems(
            Update,
            (|mut messages: MessageReader<StateTreeActivityLifecycle<Other, OtherWork>>,
              mut run: ResMut<OtherRun>| {
                for message in messages.read() {
                    if let StateTreeActivityLifecycle::Started { token, .. } = message {
                        run.0 = Some(*token);
                    }
                }
            })
            .in_set(StateTreeSet::HandleActivities),
        );
    let original = install(&mut app, definition());
    let mut builder = StateTreeBuilder::new("second-worker");
    let root = builder.compound_root("root").unwrap();
    let active = builder.atomic("active", root).unwrap();
    builder.initial(root, active).unwrap();
    builder
        .add_activity(active, StateTreeActivityDefinition::new("job", "work"))
        .unwrap();
    let handle = app
        .world_mut()
        .resource_mut::<Assets<StateTreeDefinition>>()
        .add(builder.build().unwrap());
    app.world_mut()
        .entity_mut(original)
        .insert(StateTreeInstance::<Other>::new(handle));
    app.world_mut().run_schedule(Update);
    let old = app.world().resource::<OtherRun>().0.unwrap();
    let checkpoint = Checkpoint::capture(app.world_mut()).unwrap();
    let mut corrupt = serde_json::to_value(&checkpoint).unwrap();
    let mut records: Vec<(i64, Vec<u8>)> =
        serde_json::from_value(corrupt["data"]["other-run"].clone()).unwrap();
    let id: usize = serde_json::from_slice(&records[0].1).unwrap();
    let wrong_id = if id == 0 { 1 } else { 0 };
    records[0].1 = serde_json::to_vec(&wrong_id).unwrap();
    corrupt["data"]["other-run"] = serde_json::to_value(records).unwrap();
    assert!(
        serde_json::from_value::<Checkpoint>(corrupt)
            .unwrap()
            .restore(app.world_mut())
            .is_err()
    );
    assert_eq!(app.world().resource::<OtherRun>().0, Some(old));
    checkpoint.restore(app.world_mut()).unwrap();
    let restored = app.world().resource::<OtherRun>().0.unwrap();
    assert_ne!(restored, old);
    assert_eq!(restored.entity(), token(&app).entity());
    assert!(
        !app.world_mut()
            .run_system_once(
                move |mut outcomes: StateTreeActivityOutcomeWriter<Other, OtherWork>| outcomes
                    .complete(old, ())
            )
            .unwrap()
    );
    assert!(
        app.world_mut()
            .run_system_once(
                move |mut outcomes: StateTreeActivityOutcomeWriter<Other, OtherWork>| outcomes
                    .complete(restored, ())
            )
            .unwrap()
    );
}

#[test]
fn null_like_resources_are_distinct_from_absent_resources() {
    #[derive(Resource, Default, Serialize, Deserialize)]
    struct Ready;
    let mut app = App::new();
    app.persist_resource::<Ready>("ready")
        .insert_resource(Ready);
    let present = Checkpoint::capture(app.world_mut()).unwrap();
    app.world_mut().remove_resource::<Ready>();
    let absent = Checkpoint::capture(app.world_mut()).unwrap();
    present.restore(app.world_mut()).unwrap();
    assert!(app.world().contains_resource::<Ready>());
    absent.restore(app.world_mut()).unwrap();
    assert!(!app.world().contains_resource::<Ready>());
}

fn evolving_definition(updated: bool) -> StateTreeDefinition {
    let body = r#"
        <datamodel>
          <data id="entries" expr="0"/><data id="completions" expr="0"/>
          <data id="log" expr="''"/><data id="optional" expr="nil"/>
        </datamodel>
        <state id="active">
          <onentry><assign location="entries" expr="entries + 1"/><bst:effect name="grant"/></onentry>
          <bst:activity id="job" name="work"/>
          <transition bst:id="finish" bst:after="10s" target="done"/>
          <transition event="kick"><assign location="log" expr="log .. tostring(_event.data)"/></transition>
        </state>
        <final id="done"/>
    "#;
    let body = if updated {
        body.replace(
            "<data id=\"entries\" expr=\"0\"/><data id=\"completions\" expr=\"0\"/>",
            "<data id=\"completions\" expr=\"900\"/><data id=\"new_value\" expr=\"entries + 10\"/><data id=\"entries\" expr=\"999\"/>",
        )
        .replace("id=\"optional\" expr=\"nil\"", "id=\"optional\" expr=\"123\"")
        .replace("<state id=\"active\">", r#"<state id="dormant"><bst:activity id="unrelated" name="work"/><transition bst:id="unrelated" bst:after="1s" target="done"/></state><state id="active">"#)
        .replace("entries + 1\"", "entries + 1000\"")
        .replace("bst:after=\"10s\"", "bst:after=\"999s\" cond=\"entries == 1\"")
        .replace("tostring(_event.data)", "tostring(_event.data * 10)")
    } else {
        body.into()
    };
    xml_definition("evolving.scxml", &body)
}

#[test]
fn changed_expressions_and_reordered_slots_restore_progress_and_use_current_rules() {
    let mut source = app();
    install(&mut source, evolving_definition(false));
    let stale_token = token(&source);
    source
        .world_mut()
        .resource_mut::<Runs>()
        .progress
        .get_mut(&stale_token)
        .unwrap()
        .work = 73;
    source
        .world_mut()
        .resource_mut::<Time>()
        .advance_by(Duration::from_secs(3));
    source.world_mut().run_schedule(Update);
    let checkpoint = Checkpoint::capture(source.world_mut()).unwrap();

    let mut target = app();
    target
        .world_mut()
        .resource_mut::<Assets<StateTreeDefinition>>()
        .add(evolving_definition(true));
    checkpoint.restore(target.world_mut()).unwrap();
    let entity = owner(&mut target);
    let restored = token(&target);
    assert_eq!(
        target.world().resource::<Runs>().progress[&restored].work,
        73
    );
    assert!(!complete(&mut target, stale_token));
    assert_eq!(data(&target, entity, "entries"), LuauValue::Integer(1));
    assert_eq!(data(&target, entity, "completions"), LuauValue::Integer(0));
    assert_eq!(data(&target, entity, "new_value"), LuauValue::Integer(11));
    assert_eq!(data(&target, entity, "optional"), LuauValue::Nil);
    assert_eq!(target.world().resource::<Observations>().starts, 0);
    assert_eq!(target.world().resource::<Observations>().grants, 0);
    kick(&mut target, entity, 4);
    target.world_mut().run_schedule(Update);
    assert_eq!(data(&target, entity, "log"), LuauValue::String("40".into()));
    target
        .world_mut()
        .resource_mut::<Time>()
        .advance_by(Duration::from_secs(6));
    target.world_mut().run_schedule(Update);
    assert_eq!(
        target
            .world()
            .get::<StateTreeInstance<Simulation>>(entity)
            .unwrap()
            .status(),
        &InstanceStatus::Running
    );
    target
        .world_mut()
        .resource_mut::<Time>()
        .advance_by(Duration::from_secs(1));
    target.world_mut().run_schedule(Update);
    assert_eq!(
        target
            .world()
            .get::<StateTreeInstance<Simulation>>(entity)
            .unwrap()
            .status(),
        &InstanceStatus::Complete
    );
}

#[test]
fn symbolic_migration_renames_state_data_and_live_activity_without_restarting_work() {
    let mut source = app();
    install(&mut source, evolving_definition(false));
    let old_token = token(&source);
    // This entity is owned exclusively by the runtime adapter, not a saved component.
    source
        .world_mut()
        .entity_mut(old_token.entity())
        .remove::<SavedOwner>();
    source
        .world_mut()
        .resource_mut::<Runs>()
        .progress
        .get_mut(&old_token)
        .unwrap()
        .work = 52;
    let checkpoint = Checkpoint::capture(source.world_mut()).unwrap();
    let mut target = app();
    target.add_plugins(
        bevy_gigasaver::SavePlugin::default()
            .version(2)
            .migrate(1, |data| {
                bevy_state_tree::persistence::migrate_runtime(data, |snapshot| {
                    for run in snapshot.runs.values_mut() {
                        if run.activity == "job" {
                            run.activity = "worker".into();
                        }
                    }
                    for domain in snapshot.domains.values_mut() {
                        for tree in &mut domain.trees {
                            let runtime = &mut tree.runtime;
                            let state = runtime.active.remove("active").unwrap();
                            runtime.active.insert("working".into(), state);
                            let value = runtime.data.remove("entries").unwrap();
                            runtime.data.insert("visits".into(), value);
                            let mut activity = runtime.activities.remove("job").unwrap();
                            activity.owner = "working".into();
                            runtime.activities.insert("worker".into(), activity);
                            let timers = runtime.timers.remove("active").unwrap();
                            runtime.timers.insert("working".into(), timers);
                        }
                    }
                    Ok(())
                })
            }),
    );
    let mut builder = StateTreeBuilder::new("evolving.scxml");
    builder
        .luau_datamodel([
            StateTreeData::new("visits"),
            StateTreeData::new("completions"),
            StateTreeData::new("log"),
            StateTreeData::new("optional"),
        ])
        .unwrap();
    let root = builder
        .compound_root("different-root-diagnostic-name")
        .unwrap();
    let active = builder.atomic("working", root).unwrap();
    let done = builder.final_state("done", root).unwrap();
    builder.initial(root, active).unwrap();
    builder
        .add_activity(active, StateTreeActivityDefinition::new("worker", "work"))
        .unwrap();
    builder
        .add_transition(
            active,
            StateTreeTransition::after(Duration::from_secs(10))
                .timer_id("finish")
                .target(done),
        )
        .unwrap();
    target
        .world_mut()
        .resource_mut::<Assets<StateTreeDefinition>>()
        .add(builder.build().unwrap());
    checkpoint.restore(target.world_mut()).unwrap();
    let entity = token(&target).entity();
    assert_eq!(data(&target, entity, "visits"), LuauValue::Integer(1));
    assert_eq!(
        target.world().resource::<Runs>().progress[&token(&target)].work,
        52
    );
    assert_eq!(target.world().resource::<Observations>().starts, 0);
    let restored_token = token(&target);
    assert!(complete(&mut target, restored_token));
}

#[test]
fn active_lifecycle_changes_require_migration_and_preserve_the_live_world_on_failure() {
    let mut source = app();
    install(&mut source, evolving_definition(false));
    let checkpoint = Checkpoint::capture(source.world_mut()).unwrap();
    for replacement in [
        r#"<state id="active"><bst:activity id="job" name="work"/><bst:activity id="new-job" name="work"/><transition bst:id="finish" bst:after="10s" target="done"/></state><final id="done"/>"#,
        r#"<state id="active"><bst:activity id="job" name="work"/><transition bst:id="finish" bst:after="10s" target="done"/><transition bst:id="new-timer" bst:after="20s" target="done"/></state><final id="done"/>"#,
    ] {
        let mut target = app();
        let unrelated = install(&mut target, definition());
        let body = format!(
            r#"<datamodel><data id="entries"/><data id="completions"/><data id="log"/><data id="optional"/></datamodel>{replacement}"#
        );
        target
            .world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .add(xml_definition("evolving.scxml", &body));
        let error = checkpoint
            .restore(target.world_mut())
            .unwrap_err()
            .to_string();
        assert!(error.contains("migration required"), "{error}");
        assert!(target.world().get_entity(unrelated).is_ok());
        assert_eq!(target.world().resource::<Observations>().starts, 1);
    }
}

#[test]
fn anonymous_states_and_unnamed_timers_cannot_be_saved() {
    for body in [
        r#"<state/>"#,
        r#"<state id="active"><transition bst:after="1s" target="done"/></state><final id="done"/>"#,
    ] {
        let mut source = app();
        install(&mut source, xml_definition("unstable.scxml", body));
        let error = Checkpoint::capture(source.world_mut())
            .unwrap_err()
            .to_string();
        assert!(error.contains("explicit"), "{error}");
    }
}

#[test]
fn sqlite_loads_changed_content_and_delivers_saved_events_under_current_rules() {
    use bevy_gigasaver::WorldSaveExt;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("evolving.sqlite");
    let mut source = app();
    source.add_plugins(bevy_gigasaver::SavePlugin::sqlite(&path));
    let entity = install(&mut source, evolving_definition(false));
    kick(&mut source, entity, 6);
    source.world_mut().save().unwrap();
    let mut target = app();
    target.add_plugins(bevy_gigasaver::SavePlugin::sqlite(&path));
    target
        .world_mut()
        .resource_mut::<Assets<StateTreeDefinition>>()
        .add(evolving_definition(true));
    target.world_mut().load().unwrap();
    let entity = owner(&mut target);
    target.world_mut().run_schedule(Update);
    assert_eq!(data(&target, entity, "log"), LuauValue::String("60".into()));
    assert_eq!(data(&target, entity, "entries"), LuauValue::Integer(1));
    assert_eq!(target.world().resource::<Observations>().starts, 0);
    assert_eq!(target.world().resource::<Observations>().grants, 0);
}

#[test]
fn new_initializer_failure_is_transactional_and_removed_data_needs_migration() {
    let mut source = app();
    install(&mut source, evolving_definition(false));
    let checkpoint = Checkpoint::capture(source.world_mut()).unwrap();
    for declarations in [
        r#"<data id="entries"/><data id="completions"/><data id="log"/><data id="optional"/><data id="broken" expr="missing.field"/>"#,
        r#"<data id="entries"/><data id="completions"/><data id="log"/>"#,
    ] {
        let mut target = app();
        let survivor = install(&mut target, definition());
        let body = format!(
            r#"<datamodel>{declarations}</datamodel><state id="active"><bst:activity id="job" name="work"/><transition bst:id="finish" bst:after="10s" target="done"/></state><final id="done"/>"#
        );
        target
            .world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .add(xml_definition("evolving.scxml", &body));
        let error = checkpoint
            .restore(target.world_mut())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("broken") || error.contains("optional"),
            "{error}"
        );
        assert_eq!(data(&target, survivor, "entries"), LuauValue::Integer(1));
        assert_eq!(target.world().resource::<Runs>().progress.len(), 1);
    }
}

#[test]
fn history_memories_survive_reordered_children_and_new_history_slots() {
    let mut source = app();
    let entity = install(
        &mut source,
        xml_definition(
            "history-evolves",
            r#"
        <state id="inside" initial="first">
          <history id="recent"><transition target="first"/></history>
          <state id="first"><transition event="kick" target="second"/></state>
          <state id="second"><transition event="kick" target="outside"/></state>
        </state>
        <state id="outside"><transition event="kick" target="recent"/></state>
    "#,
        ),
    );
    for _ in 0..2 {
        kick(&mut source, entity, 1);
        source.world_mut().run_schedule(Update);
    }
    let checkpoint = Checkpoint::capture(source.world_mut()).unwrap();
    let mut target = app();
    target.world_mut().resource_mut::<Assets<StateTreeDefinition>>().add(xml_definition("history-evolves", r#"
        <state id="unused"><history id="unrelated"><transition target="unused-child"/></history><state id="unused-child"/></state>
        <state id="inside" initial="first">
          <history id="recent"><transition target="first"/></history>
          <state id="second"><transition event="kick" target="outside"/></state>
          <state id="first"><transition event="kick" target="second"/></state>
        </state>
        <state id="outside"><transition event="kick" target="recent"/></state>
    "#));
    checkpoint.restore(target.world_mut()).unwrap();
    let entity = owner(&mut target);
    kick(&mut target, entity, 1);
    target.world_mut().run_schedule(Update);
    assert_eq!(
        target
            .world()
            .get::<StateTreeInstance<Simulation>>(entity)
            .unwrap()
            .active_state_names()
            .last(),
        Some("second")
    );
}

struct RefactoredDomain;
struct RefactoredWork;
impl StateTreeActivity for RefactoredWork {
    type Output = ();
    type Error = ();
}
impl StateTreeDomain for RefactoredDomain {
    const AUTHORED_NAME: &'static str = "save-test";
    fn configure(config: &mut StateTreeConfig<'_, Self>) {
        config.activity("work", |_, _| Ok(RefactoredWork));
    }
}
#[derive(Resource, Default, Serialize, Deserialize)]
struct RefactoredRuns {
    progress: HashMap<StateTreeActivityToken<RefactoredDomain, RefactoredWork>, Progress>,
    nested: Vec<Option<StateTreeActivityToken<RefactoredDomain, RefactoredWork>>>,
}

#[test]
fn rust_activity_type_names_are_not_save_identities() {
    let mut source = app();
    let mut builder = StateTreeBuilder::new("type-refactor");
    let root = builder.compound_root("root").unwrap();
    let active = builder.atomic("active", root).unwrap();
    builder.initial(root, active).unwrap();
    builder
        .add_activity(active, StateTreeActivityDefinition::new("job", "work"))
        .unwrap();
    let definition = builder.build().unwrap();
    install(&mut source, definition.clone());
    let old = token(&source);
    source
        .world_mut()
        .resource_mut::<Runs>()
        .progress
        .get_mut(&old)
        .unwrap()
        .work = 18;
    let checkpoint = Checkpoint::capture(source.world_mut()).unwrap();
    let mut target = App::new();
    target
        .add_plugins((
            TaskPoolPlugin::default(),
            TimePlugin,
            AssetPlugin::default(),
            StateTreePlugin::<RefactoredDomain>::new(Update),
        ))
        .init_resource::<RefactoredRuns>()
        .persist_resource::<RefactoredRuns>("runs")
        .persist_component::<SavedOwner>("owners");
    target
        .world_mut()
        .resource_mut::<Assets<StateTreeDefinition>>()
        .add(definition);
    checkpoint.restore(target.world_mut()).unwrap();
    let runs = target.world().resource::<RefactoredRuns>();
    let new = *runs.progress.keys().next().unwrap();
    assert_eq!(runs.progress[&new].work, 18);
    assert_eq!(runs.nested, [Some(new)]);
    assert!(
        target
            .world()
            .get::<StateTreeInstance<RefactoredDomain>>(new.entity())
            .is_some()
    );
}
