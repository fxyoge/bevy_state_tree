use bevy_app::{App, Update};
use bevy_asset::AssetPlugin;
use bevy_ecs::{event::Event, message::Message};
use bevy_state_tree::{
    StateTreeAction, StateTreeActivity, StateTreeActivityDefinition, StateTreeBuilder,
    StateTreeConfig, StateTreeDefinition, StateTreeEvent, StateTreePlugin,
    StateTreeProtocolInventory, StateTreeProtocolIssue, StateTreeTransition,
    analyze_state_tree_protocol,
};
use bevy_time::TimePlugin;

fn configure(config: &mut StateTreeConfig<'_>) {
    config
        .event::<UsedEvent>()
        .event::<UnusedEvent>()
        .effect("used.effect", |_, _| Ok(EffectMessage))
        .effect("unused.effect", |_, _| Ok(EffectMessage))
        .activity("used.activity", |_, _| Ok(TestActivity))
        .activity("unused.activity", |_, _| Ok(TestActivity));
}

#[derive(Event, StateTreeEvent)]
#[state_tree(name = "used.event", broadcast)]
struct UsedEvent;
#[derive(Event, StateTreeEvent)]
#[state_tree(name = "unused.event", broadcast)]
struct UnusedEvent;
#[derive(Message)]
struct EffectMessage;
struct TestActivity;
impl StateTreeActivity for TestActivity {
    type Output = ();
    type Error = ();
}

fn app() -> App {
    let mut app = App::new();
    app.add_plugins((
        TimePlugin,
        AssetPlugin::default(),
        StateTreePlugin::new(Update).configure(configure),
    ));
    app
}

fn definition(
    name: &str,
    event: Option<&str>,
    effect: Option<&str>,
    activity: Option<&str>,
    internal_event: Option<&str>,
) -> StateTreeDefinition {
    let mut builder = StateTreeBuilder::new(name);
    let root = builder.compound_root("root").unwrap();
    let active = builder.atomic("active", root).unwrap();
    builder.initial(root, active).unwrap();
    if let Some(event) = event {
        builder
            .add_transition(active, StateTreeTransition::on(event))
            .unwrap();
    }
    let mut actions = effect
        .map(|name| StateTreeAction::effect(name, []))
        .into_iter()
        .collect::<Vec<_>>();
    if let Some(event) = internal_event {
        actions.push(StateTreeAction::raise(event));
        builder
            .add_transition(active, StateTreeTransition::on(event))
            .unwrap();
    }
    builder.entry_actions(active, actions).unwrap();
    if let Some(activity) = activity {
        builder
            .add_activity(
                active,
                StateTreeActivityDefinition::new("request", activity),
            )
            .unwrap();
    }
    builder.build().unwrap()
}

#[test]
fn inventory_and_generated_usage_cover_all_registration_kinds() {
    let app = app();
    let inventory = app.world().resource::<StateTreeProtocolInventory>();
    assert_eq!(
        inventory.events().collect::<Vec<_>>(),
        ["unused.event", "used.event"]
    );
    assert_eq!(
        inventory.effects().collect::<Vec<_>>(),
        ["unused.effect", "used.effect"]
    );
    assert_eq!(
        inventory.activities().collect::<Vec<_>>(),
        ["bst.spawn", "unused.activity", "used.activity"]
    );
    let generated = definition(
        "generated",
        Some("used.event"),
        Some("used.effect"),
        Some("used.activity"),
        None,
    );
    assert_eq!(
        analyze_state_tree_protocol(inventory, [("generated", &generated)]).issues(),
        [
            StateTreeProtocolIssue::UnusedEvent {
                event: "unused.event".into()
            },
            StateTreeProtocolIssue::UnusedEffect {
                effect: "unused.effect".into()
            },
            StateTreeProtocolIssue::UnusedActivity {
                activity: "unused.activity".into()
            },
        ]
    );
}

#[test]
fn built_in_activity_validates_but_is_never_reported_unused() {
    let app = app();
    let inventory = app.world().resource::<StateTreeProtocolInventory>();
    let spawn = definition("spawn", None, None, Some("bst.spawn"), None);
    let analysis = analyze_state_tree_protocol(inventory, [("spawn", &spawn)]);
    assert!(!analysis.issues().iter().any(|issue| matches!(issue,
        StateTreeProtocolIssue::MissingActivity { activity, .. } | StateTreeProtocolIssue::UnusedActivity { activity }
        if activity.as_ref() == "bst.spawn")));
}

#[test]
fn missing_registrations_are_reported_without_requiring_internal_events() {
    let app = app();
    let inventory = app.world().resource::<StateTreeProtocolInventory>();
    let missing = definition(
        "missing",
        Some("missing.event"),
        Some("missing.effect"),
        Some("missing.activity"),
        Some("local.event"),
    );
    let analysis = analyze_state_tree_protocol(inventory, [("missing.asset", &missing)]);
    for expected in [
        StateTreeProtocolIssue::MissingEvent {
            label: "missing.asset".into(),
            event: "missing.event".into(),
        },
        StateTreeProtocolIssue::MissingEffect {
            label: "missing.asset".into(),
            effect: "missing.effect".into(),
        },
        StateTreeProtocolIssue::MissingActivity {
            label: "missing.asset".into(),
            activity: "missing.activity".into(),
        },
    ] {
        assert!(analysis.issues().contains(&expected));
    }
    assert!(!analysis.issues().iter().any(|issue| matches!(issue,
        StateTreeProtocolIssue::MissingEvent { event, .. } if event.as_ref() == "local.event")));
}

#[test]
fn results_are_deterministic_and_duplicate_references_do_not_duplicate_findings() {
    let app = app();
    let inventory = app.world().resource::<StateTreeProtocolInventory>();
    let used = definition(
        "used",
        Some("used.event"),
        Some("used.effect"),
        Some("used.activity"),
        None,
    );
    let missing = definition("missing", Some("missing.event"), None, None, None);
    let first = analyze_state_tree_protocol(
        inventory,
        [
            ("used", &used),
            ("missing", &missing),
            ("missing", &missing),
        ],
    );
    let second = analyze_state_tree_protocol(inventory, [("missing", &missing), ("used", &used)]);
    assert_eq!(first, second);
    assert_eq!(
        first
            .issues()
            .iter()
            .filter(|issue| matches!(issue, StateTreeProtocolIssue::MissingEvent { .. }))
            .count(),
        1
    );
}
