use std::collections::BTreeSet;

use bevy_app::App;
use bevy_ecs::prelude::Resource;

use crate::{
    StateTreeDefinition, activity::StateTreeActivityRegistry, effect::StateTreeEffectRegistry,
    event::StateTreeAppEventRegistry,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RegistrationOrigin {
    Application,
    BuiltIn,
}

#[derive(Clone, Debug)]
struct Registration {
    name: Box<str>,
    origin: RegistrationOrigin,
}

/// Read-only inventory of the application's installed state-tree protocol.
#[derive(Clone, Debug, Resource)]
pub struct StateTreeProtocolInventory {
    events: Box<[Registration]>,
    effects: Box<[Registration]>,
    activities: Box<[Registration]>,
}

impl StateTreeProtocolInventory {
    /// Iterates over the exact registered external event names.
    pub fn events(&self) -> impl ExactSizeIterator<Item = &str> {
        self.events.iter().map(|entry| entry.name.as_ref())
    }

    /// Iterates over the exact registered effect names.
    pub fn effects(&self) -> impl ExactSizeIterator<Item = &str> {
        self.effects.iter().map(|entry| entry.name.as_ref())
    }

    /// Iterates over the exact registered activity names, including built-ins.
    pub fn activities(&self) -> impl ExactSizeIterator<Item = &str> {
        self.activities.iter().map(|entry| entry.name.as_ref())
    }
}

pub(crate) fn install(app: &mut App) {
    let mut events = app
        .world()
        .resource::<StateTreeAppEventRegistry>()
        .registrations()
        .map(registration)
        .collect::<Vec<_>>();
    let mut effects = app
        .world()
        .resource::<StateTreeEffectRegistry>()
        .registrations()
        .map(registration)
        .collect::<Vec<_>>();
    let mut activities = app
        .world()
        .resource::<StateTreeActivityRegistry>()
        .registrations()
        .map(registration)
        .collect::<Vec<_>>();
    events.sort_by(|left, right| left.name.cmp(&right.name));
    effects.sort_by(|left, right| left.name.cmp(&right.name));
    activities.sort_by(|left, right| left.name.cmp(&right.name));
    // Freeze before execution: dense event indices and inventory describe the
    // same complete protocol for every definition in the application.
    app.world()
        .resource::<StateTreeAppEventRegistry>()
        .protocol();
    app.insert_resource(StateTreeProtocolInventory {
        events: events.into_boxed_slice(),
        effects: effects.into_boxed_slice(),
        activities: activities.into_boxed_slice(),
    });
}

fn registration((name, origin): (&str, RegistrationOrigin)) -> Registration {
    Registration {
        name: name.into(),
        origin,
    }
}

/// A read-only view of the authored protocol used by one compiled definition.
#[derive(Clone, Copy)]
pub struct StateTreeDefinitionProtocol<'a> {
    pub(crate) definition: &'a StateTreeDefinition,
}

impl<'a> StateTreeDefinitionProtocol<'a> {
    /// Iterates over exact event names required by transition descriptors.
    pub fn external_events(&self) -> impl Iterator<Item = &'a str> {
        self.definition.compiled.protocol_event_requirements()
    }

    /// Iterates over exact effect names used by the definition.
    pub fn effects(&self) -> impl ExactSizeIterator<Item = &'a str> {
        self.definition.compiled.effect_names()
    }

    /// Iterates over activity names used by the definition.
    pub fn activities(&self) -> impl ExactSizeIterator<Item = &'a str> {
        self.definition.compiled.protocol_activity_names()
    }

    /// Iterates over exact event names emitted internally by the definition.
    pub fn internal_events(&self) -> impl Iterator<Item = &'a str> {
        self.definition.compiled.protocol_internal_event_names()
    }
}

/// One deterministic protocol-analysis finding.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum StateTreeProtocolIssue {
    MissingEvent { label: Box<str>, event: Box<str> },
    MissingEffect { label: Box<str>, effect: Box<str> },
    MissingActivity { label: Box<str>, activity: Box<str> },
    UnusedEvent { event: Box<str> },
    UnusedEffect { effect: Box<str> },
    UnusedActivity { activity: Box<str> },
}

/// Deterministically ordered findings from state-tree protocol analysis.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StateTreeProtocolAnalysis {
    issues: Box<[StateTreeProtocolIssue]>,
}

impl StateTreeProtocolAnalysis {
    pub fn issues(&self) -> &[StateTreeProtocolIssue] {
        &self.issues
    }
    pub fn is_empty(&self) -> bool {
        self.issues.is_empty()
    }
}

/// Analyzes compiled definitions against the installed application protocol.
/// Labels are caller-owned diagnostic identifiers such as asset paths.
pub fn analyze_state_tree_protocol<'a, L, I>(
    inventory: &StateTreeProtocolInventory,
    definitions: I,
) -> StateTreeProtocolAnalysis
where
    L: Into<Box<str>>,
    I: IntoIterator<Item = (L, &'a StateTreeDefinition)>,
{
    let mut issues = BTreeSet::new();
    let mut used_events = BTreeSet::new();
    let mut used_effects = BTreeSet::new();
    let mut used_activities = BTreeSet::new();
    let registered_events = inventory.events().collect::<BTreeSet<_>>();
    let registered_effects = inventory.effects().collect::<BTreeSet<_>>();
    let registered_activities = inventory.activities().collect::<BTreeSet<_>>();

    for (label, definition) in definitions {
        let label = label.into();
        let protocol = definition.protocol();
        let internal_events = protocol.internal_events().collect::<BTreeSet<_>>();
        for event in protocol.external_events() {
            if registered_events.contains(event) {
                used_events.insert(event);
            } else if !internal_events.contains(event) {
                issues.insert(StateTreeProtocolIssue::MissingEvent {
                    label: label.clone(),
                    event: event.into(),
                });
            }
        }
        for effect in protocol.effects() {
            if registered_effects.contains(effect) {
                used_effects.insert(effect);
            } else {
                issues.insert(StateTreeProtocolIssue::MissingEffect {
                    label: label.clone(),
                    effect: effect.into(),
                });
            }
        }
        for activity in protocol.activities() {
            if registered_activities.contains(activity) {
                used_activities.insert(activity);
            } else {
                issues.insert(StateTreeProtocolIssue::MissingActivity {
                    label: label.clone(),
                    activity: activity.into(),
                });
            }
        }
    }
    for entry in inventory
        .events
        .iter()
        .filter(|entry| entry.origin == RegistrationOrigin::Application)
    {
        if !used_events.contains(entry.name.as_ref()) {
            issues.insert(StateTreeProtocolIssue::UnusedEvent {
                event: entry.name.clone(),
            });
        }
    }
    for entry in inventory
        .effects
        .iter()
        .filter(|entry| entry.origin == RegistrationOrigin::Application)
    {
        if !used_effects.contains(entry.name.as_ref()) {
            issues.insert(StateTreeProtocolIssue::UnusedEffect {
                effect: entry.name.clone(),
            });
        }
    }
    for entry in inventory
        .activities
        .iter()
        .filter(|entry| entry.origin == RegistrationOrigin::Application)
    {
        if !used_activities.contains(entry.name.as_ref()) {
            issues.insert(StateTreeProtocolIssue::UnusedActivity {
                activity: entry.name.clone(),
            });
        }
    }
    StateTreeProtocolAnalysis {
        issues: issues.into_iter().collect(),
    }
}
