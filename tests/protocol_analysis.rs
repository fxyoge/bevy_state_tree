use bevy_app::{App, Update};
use bevy_asset::AssetPlugin;
use bevy_ecs::{event::Event, message::Message};
use bevy_state_tree::{
    StateTreeAction, StateTreeActivity, StateTreeActivityDefinition, StateTreeBuilder,
    StateTreeConfig, StateTreeDefinition, StateTreeDomain, StateTreeDomainRegistry, StateTreeEvent,
    StateTreePlugin, StateTreeProtocolIssue, StateTreeTransition, analyze_state_tree_protocol,
};
use bevy_time::TimePlugin;

struct TestDomain;

impl StateTreeDomain for TestDomain {
    const AUTHORED_NAME: &'static str = "test";

    fn configure(config: &mut StateTreeConfig<'_, Self>) {
        config
            .event::<UsedEvent>()
            .event::<UnusedEvent>()
            .effect::<EffectMessage, _>("used.effect", |_context, _arguments| Ok(EffectMessage))
            .effect::<EffectMessage, _>("unused.effect", |_context, _arguments| Ok(EffectMessage))
            .activity::<TestActivity, _>("used.activity", |_context, _arguments| Ok(TestActivity))
            .activity::<TestActivity, _>("unused.activity", |_context, _arguments| {
                Ok(TestActivity)
            });
    }
}

#[derive(Event, StateTreeEvent)]
#[state_tree(domain = TestDomain, name = "used.event", broadcast)]
struct UsedEvent;

#[derive(Event, StateTreeEvent)]
#[state_tree(domain = TestDomain, name = "unused.event", broadcast)]
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
    app.add_plugins((TimePlugin, AssetPlugin::default()));
    app.add_plugins(StateTreePlugin::<TestDomain>::new(Update));
    app
}

fn definition(
    name: &str,
    domain: Option<&str>,
    event: Option<&str>,
    effect: Option<&str>,
    activity: Option<&str>,
    internal_event: Option<&str>,
) -> StateTreeDefinition {
    let mut builder = StateTreeBuilder::new(name);
    if let Some(domain) = domain {
        builder.domain(domain);
    }
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
fn inventory_and_generated_definition_usage_cover_all_registration_kinds() {
    let app = app();
    let registry = app.world().resource::<StateTreeDomainRegistry>();
    let domain = registry.get("test").unwrap();
    assert_eq!(domain.authored_name(), "test");
    assert_eq!(
        domain.events().collect::<Vec<_>>(),
        ["unused.event", "used.event"]
    );
    assert_eq!(
        domain.effects().collect::<Vec<_>>(),
        ["unused.effect", "used.effect"]
    );
    assert_eq!(
        domain.activities().collect::<Vec<_>>(),
        ["bst.spawn", "unused.activity", "used.activity"]
    );

    let generated = definition(
        "generated",
        Some("test"),
        Some("used.event"),
        Some("used.effect"),
        Some("used.activity"),
        None,
    );
    let analysis = analyze_state_tree_protocol(registry, [("generated", &generated)]);
    assert_eq!(
        analysis.issues(),
        [
            StateTreeProtocolIssue::UnusedEvent {
                domain: "test".into(),
                event: "unused.event".into(),
            },
            StateTreeProtocolIssue::UnusedEffect {
                domain: "test".into(),
                effect: "unused.effect".into(),
            },
            StateTreeProtocolIssue::UnusedActivity {
                domain: "test".into(),
                activity: "unused.activity".into(),
            },
        ]
    );
}

#[test]
fn built_in_activity_validates_but_is_never_reported_unused() {
    let app = app();
    let registry = app.world().resource::<StateTreeDomainRegistry>();
    let spawn = definition("spawn", Some("test"), None, None, Some("bst.spawn"), None);
    let analysis = analyze_state_tree_protocol(registry, [("spawn", &spawn)]);
    assert!(!analysis.issues().iter().any(|issue| matches!(
        issue,
        StateTreeProtocolIssue::MissingActivity { activity, .. }
            | StateTreeProtocolIssue::UnusedActivity { activity, .. }
            if activity.as_ref() == "bst.spawn"
    )));
}

#[test]
fn reports_domain_and_missing_registration_issues_without_requiring_internal_events() {
    let app = app();
    let registry = app.world().resource::<StateTreeDomainRegistry>();
    let domainless = definition("domainless", None, None, None, None, None);
    let unknown = definition("unknown", Some("other"), None, None, None, None);
    let missing = definition(
        "missing",
        Some("test"),
        Some("missing.event"),
        Some("missing.effect"),
        Some("missing.activity"),
        Some("local.event"),
    );

    let analysis = analyze_state_tree_protocol(
        registry,
        [
            ("unknown.asset", &unknown),
            ("missing.asset", &missing),
            ("domainless.asset", &domainless),
        ],
    );
    assert!(
        analysis
            .issues()
            .contains(&StateTreeProtocolIssue::DomainlessDefinition {
                label: "domainless.asset".into(),
            })
    );
    assert!(
        analysis
            .issues()
            .contains(&StateTreeProtocolIssue::UnknownDomain {
                label: "unknown.asset".into(),
                domain: "other".into(),
            })
    );
    assert!(
        analysis
            .issues()
            .contains(&StateTreeProtocolIssue::MissingEvent {
                label: "missing.asset".into(),
                domain: "test".into(),
                event: "missing.event".into(),
            })
    );
    assert!(
        analysis
            .issues()
            .contains(&StateTreeProtocolIssue::MissingEffect {
                label: "missing.asset".into(),
                domain: "test".into(),
                effect: "missing.effect".into(),
            })
    );
    assert!(
        analysis
            .issues()
            .contains(&StateTreeProtocolIssue::MissingActivity {
                label: "missing.asset".into(),
                domain: "test".into(),
                activity: "missing.activity".into(),
            })
    );
    assert!(!analysis.issues().iter().any(|issue| matches!(
        issue,
        StateTreeProtocolIssue::MissingEvent { event, .. } if event.as_ref() == "local.event"
    )));
}

#[test]
fn results_are_deterministic_and_duplicate_references_do_not_duplicate_usage() {
    let app = app();
    let registry = app.world().resource::<StateTreeDomainRegistry>();
    let used = definition(
        "used",
        Some("test"),
        Some("used.event"),
        Some("used.effect"),
        Some("used.activity"),
        None,
    );
    let domainless = definition("domainless", None, None, None, None, None);

    let first = analyze_state_tree_protocol(
        registry,
        [
            ("used.asset", &used),
            ("domainless.asset", &domainless),
            ("used.asset", &used),
        ],
    );
    let second = analyze_state_tree_protocol(
        registry,
        [
            ("used.asset", &used),
            ("used.asset", &used),
            ("domainless.asset", &domainless),
        ],
    );
    assert_eq!(first, second);
    assert_eq!(
        first
            .issues()
            .iter()
            .filter(|issue| matches!(issue, StateTreeProtocolIssue::DomainlessDefinition { .. }))
            .count(),
        1
    );
}
