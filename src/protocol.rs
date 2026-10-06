use std::collections::{BTreeMap, BTreeSet, HashSet};

use bevy_app::App;
use bevy_ecs::prelude::Resource;

use crate::{
    StateTreeDefinition, StateTreeDomain, activity::StateTreeActivityRegistry,
    effect::StateTreeEffectRegistry, event::StateTreeAppEventRegistry,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RegistrationOrigin {
    Domain,
    BuiltIn,
}

#[derive(Clone, Debug)]
struct Registration {
    name: Box<str>,
    origin: RegistrationOrigin,
}

/// Read-only inventory of one installed state-tree domain's authored protocol.
#[derive(Clone, Debug)]
pub struct StateTreeDomainInventory {
    authored_name: &'static str,
    events: Box<[Registration]>,
    effects: Box<[Registration]>,
    activities: Box<[Registration]>,
}

impl StateTreeDomainInventory {
    /// Returns the domain's unique authored name.
    pub fn authored_name(&self) -> &'static str {
        self.authored_name
    }

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

/// Read-only resource containing every installed state-tree domain protocol.
#[derive(Default, Resource)]
pub struct StateTreeDomainRegistry {
    reserved_names: HashSet<&'static str>,
    domains: BTreeMap<&'static str, StateTreeDomainInventory>,
}

impl StateTreeDomainRegistry {
    /// Returns an installed domain by its authored name.
    pub fn get(&self, authored_name: &str) -> Option<&StateTreeDomainInventory> {
        self.domains.get(authored_name)
    }

    /// Iterates over installed domains in authored-name order.
    pub fn domains(&self) -> impl ExactSizeIterator<Item = &StateTreeDomainInventory> {
        self.domains.values()
    }

    pub(crate) fn reserve(&mut self, authored_name: &'static str) -> bool {
        self.reserved_names.insert(authored_name)
    }

    fn install(&mut self, inventory: StateTreeDomainInventory) {
        self.domains.insert(inventory.authored_name, inventory);
    }
}

pub(crate) fn install_domain<D: StateTreeDomain>(app: &mut App) {
    let mut events = app
        .world()
        .resource::<StateTreeAppEventRegistry<D>>()
        .registrations()
        .map(registration)
        .collect::<Vec<_>>();
    let mut effects = app
        .world()
        .resource::<StateTreeEffectRegistry<D>>()
        .registrations()
        .map(registration)
        .collect::<Vec<_>>();
    let mut activities = app
        .world()
        .resource::<StateTreeActivityRegistry<D>>()
        .registrations()
        .map(registration)
        .collect::<Vec<_>>();
    events.sort_by(|left, right| left.name.cmp(&right.name));
    effects.sort_by(|left, right| left.name.cmp(&right.name));
    activities.sort_by(|left, right| left.name.cmp(&right.name));

    app.world_mut()
        .resource_mut::<StateTreeDomainRegistry>()
        .install(StateTreeDomainInventory {
            authored_name: D::AUTHORED_NAME,
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
    /// Returns the definition's declared domain, if present.
    pub fn domain(&self) -> Option<&'a str> {
        self.definition.domain()
    }

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
    DomainlessDefinition {
        label: Box<str>,
    },
    UnknownDomain {
        label: Box<str>,
        domain: Box<str>,
    },
    MissingEvent {
        label: Box<str>,
        domain: Box<str>,
        event: Box<str>,
    },
    MissingEffect {
        label: Box<str>,
        domain: Box<str>,
        effect: Box<str>,
    },
    MissingActivity {
        label: Box<str>,
        domain: Box<str>,
        activity: Box<str>,
    },
    UnusedEvent {
        domain: Box<str>,
        event: Box<str>,
    },
    UnusedEffect {
        domain: Box<str>,
        effect: Box<str>,
    },
    UnusedActivity {
        domain: Box<str>,
        activity: Box<str>,
    },
}

/// Deterministically ordered findings from state-tree protocol analysis.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StateTreeProtocolAnalysis {
    issues: Box<[StateTreeProtocolIssue]>,
}

impl StateTreeProtocolAnalysis {
    /// Returns all findings in deterministic order.
    pub fn issues(&self) -> &[StateTreeProtocolIssue] {
        &self.issues
    }

    /// Returns whether no findings were produced.
    pub fn is_empty(&self) -> bool {
        self.issues.is_empty()
    }
}

/// Analyzes compiled definitions against installed domain inventories.
///
/// Labels are caller-owned diagnostic identifiers such as asset paths. Repeated
/// labels and definition references do not duplicate findings or usage.
pub fn analyze_state_tree_protocol<'a, L, I>(
    registry: &StateTreeDomainRegistry,
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

    for (label, definition) in definitions {
        let label = label.into();
        let protocol = definition.protocol();
        let Some(domain_name) = protocol.domain() else {
            issues.insert(StateTreeProtocolIssue::DomainlessDefinition { label });
            continue;
        };
        let Some(domain) = registry.get(domain_name) else {
            issues.insert(StateTreeProtocolIssue::UnknownDomain {
                label,
                domain: domain_name.into(),
            });
            continue;
        };
        let internal_events = protocol.internal_events().collect::<BTreeSet<_>>();
        let registered_events = domain.events().collect::<BTreeSet<_>>();
        let registered_effects = domain.effects().collect::<BTreeSet<_>>();
        let registered_activities = domain.activities().collect::<BTreeSet<_>>();

        for event in protocol.external_events() {
            if registered_events.contains(event) {
                used_events.insert((domain_name, event));
            } else if !internal_events.contains(event) {
                issues.insert(StateTreeProtocolIssue::MissingEvent {
                    label: label.clone(),
                    domain: domain_name.into(),
                    event: event.into(),
                });
            }
        }
        for effect in protocol.effects() {
            if registered_effects.contains(effect) {
                used_effects.insert((domain_name, effect));
            } else {
                issues.insert(StateTreeProtocolIssue::MissingEffect {
                    label: label.clone(),
                    domain: domain_name.into(),
                    effect: effect.into(),
                });
            }
        }
        for activity in protocol.activities() {
            if registered_activities.contains(activity) {
                used_activities.insert((domain_name, activity));
            } else {
                issues.insert(StateTreeProtocolIssue::MissingActivity {
                    label: label.clone(),
                    domain: domain_name.into(),
                    activity: activity.into(),
                });
            }
        }
    }

    for domain in registry.domains() {
        for entry in domain
            .events
            .iter()
            .filter(|entry| entry.origin == RegistrationOrigin::Domain)
        {
            if !used_events.contains(&(domain.authored_name, entry.name.as_ref())) {
                issues.insert(StateTreeProtocolIssue::UnusedEvent {
                    domain: domain.authored_name.into(),
                    event: entry.name.clone(),
                });
            }
        }
        for entry in domain
            .effects
            .iter()
            .filter(|entry| entry.origin == RegistrationOrigin::Domain)
        {
            if !used_effects.contains(&(domain.authored_name, entry.name.as_ref())) {
                issues.insert(StateTreeProtocolIssue::UnusedEffect {
                    domain: domain.authored_name.into(),
                    effect: entry.name.clone(),
                });
            }
        }
        for entry in domain
            .activities
            .iter()
            .filter(|entry| entry.origin == RegistrationOrigin::Domain)
        {
            if !used_activities.contains(&(domain.authored_name, entry.name.as_ref())) {
                issues.insert(StateTreeProtocolIssue::UnusedActivity {
                    domain: domain.authored_name.into(),
                    activity: entry.name.clone(),
                });
            }
        }
    }

    StateTreeProtocolAnalysis {
        issues: issues.into_iter().collect(),
    }
}
