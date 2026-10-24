use std::{
    any::TypeId,
    collections::HashSet,
    fmt,
    hash::{Hash, Hasher},
    marker::PhantomData,
    sync::Arc,
};

use bevy_app::App;
use bevy_ecs::{
    entity::Entity,
    message::MessageReader,
    prelude::{Commands, Component, Query, Resource},
    schedule::{InternedScheduleLabel, IntoScheduleConfigs, SystemSet},
    system::{In, IntoSystem},
    world::World,
};

use bevy_ecs::prelude::Local;

use crate::{
    EntityStateTreeEvent, EntityStateTreeEventWriter, StateTreeActivity, StateTreeActivityArgs,
    StateTreeActivityContext, StateTreeActivityError, StateTreeActivityLifecycle,
    StateTreeActivityToken, StateTreeInstance, StateTreeSet,
};

/// Describes one global value observed by state-tree activities.
///
/// A global binding source is an ordinary no-input Bevy system that returns
/// [`Self::Value`]. The source runs only while at least one active activity is
/// listening. Values are compared with [`PartialEq`], so event delivery follows
/// semantic value changes rather than Bevy component change ticks.
pub trait StateTreeBinding: Send + Sync + 'static {
    /// The complete value sampled from the backing Bevy world.
    type Value: PartialEq + Send + Sync + 'static;
    /// The entity-targeted event delivered to each listening state tree.
    type Event: EntityStateTreeEvent;

    /// Creates the event for one listening state-tree entity.
    fn event(&self, entity: Entity, value: &Self::Value) -> Self::Event;
}

/// Typed ordering point for one registered global binding source.
///
/// Systems that mutate a binding's backing data in the same schedule can run
/// before this set to make the new value visible to that schedule's sample.
#[derive(SystemSet)]
pub struct StateTreeBindingSet<B: Send + Sync + 'static> {
    marker: PhantomData<fn() -> B>,
}

impl<B: Send + Sync + 'static> Copy for StateTreeBindingSet<B> {}

impl<B: Send + Sync + 'static> Clone for StateTreeBindingSet<B> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<B: Send + Sync + 'static> fmt::Debug for StateTreeBindingSet<B> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("StateTreeBindingSet")
            .field(&std::any::type_name::<B>())
            .finish()
    }
}

impl<B: Send + Sync + 'static> PartialEq for StateTreeBindingSet<B> {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl<B: Send + Sync + 'static> Eq for StateTreeBindingSet<B> {}

impl<B: Send + Sync + 'static> Hash for StateTreeBindingSet<B> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        TypeId::of::<B>().hash(state);
    }
}

impl<B: Send + Sync + 'static> StateTreeBindingSet<B> {
    /// Returns the ordering set for binding type `B`.
    pub const fn new() -> Self {
        Self {
            marker: PhantomData,
        }
    }
}

impl<B: Send + Sync + 'static> Default for StateTreeBindingSet<B> {
    fn default() -> Self {
        Self::new()
    }
}

struct GlobalBindingActivity<B>(Arc<B>);

impl<B: Send + Sync + 'static> StateTreeActivity for GlobalBindingActivity<B> {
    type Output = ();
    type Error = ();
}

struct GlobalBindingRun<B: StateTreeBinding> {
    token: StateTreeActivityToken<GlobalBindingActivity<B>>,
    binding: Arc<B>,
    needs_initial: bool,
}

#[derive(Component)]
#[component(storage = "SparseSet")]
struct GlobalBindingListeners<B: StateTreeBinding> {
    runs: Vec<GlobalBindingRun<B>>,
}

type PrunableGlobalBindings<'w, 's, B> = Query<
    'w,
    's,
    (
        Entity,
        Option<&'static StateTreeInstance>,
        &'static mut GlobalBindingListeners<B>,
    ),
>;

#[derive(Resource, Default)]
struct RegisteredGlobalBindings {
    types: HashSet<TypeId>,
}

pub(crate) fn register_global_binding<B, C, S, M>(
    app: &mut App,
    schedule: InternedScheduleLabel,
    name: Box<str>,
    constructor: C,
    source: S,
) where
    B: StateTreeBinding,
    C: Fn(
            StateTreeActivityContext<'_>,
            &mut StateTreeActivityArgs,
        ) -> Result<B, StateTreeActivityError>
        + Send
        + Sync
        + 'static,
    S: IntoSystem<(), B::Value, M> + 'static,
{
    app.init_resource::<RegisteredGlobalBindings>();
    let inserted = app
        .world_mut()
        .resource_mut::<RegisteredGlobalBindings>()
        .types
        .insert(TypeId::of::<B>());
    assert!(
        inserted,
        "state-tree global binding type `{}` was registered more than once",
        std::any::type_name::<B>()
    );

    crate::event::ensure_event::<B::Event>(app, crate::protocol::RegistrationOrigin::Application);
    crate::activity::register_activity::<GlobalBindingActivity<B>, _>(
        app,
        name,
        move |context, arguments| {
            constructor(context, arguments).map(|binding| GlobalBindingActivity(Arc::new(binding)))
        },
        crate::protocol::RegistrationOrigin::Application,
    );
    app.add_systems(
        schedule,
        source
            .pipe(dispatch_global_binding::<B>)
            .run_if(has_active_global_binding::<B>)
            .in_set(StateTreeSet::QueueEvents)
            .in_set(StateTreeBindingSet::<B>::new()),
    )
    .add_systems(
        schedule,
        (manage_global_binding::<B>, prune_global_binding::<B>)
            .chain()
            .in_set(StateTreeSet::HandleActivities),
    );
}

fn has_active_global_binding<B>(
    listeners: Query<(&StateTreeInstance, &GlobalBindingListeners<B>)>,
) -> bool
where
    B: StateTreeBinding,
{
    listeners.iter().any(|(instance, listeners)| {
        listeners
            .runs
            .iter()
            .any(|run| instance.accepts_activity_token(run.token.erased()))
    })
}

fn manage_global_binding<B>(
    mut lifecycle: MessageReader<StateTreeActivityLifecycle<GlobalBindingActivity<B>>>,
    mut commands: Commands,
) where
    B: StateTreeBinding,
{
    for lifecycle in lifecycle.read() {
        match lifecycle {
            StateTreeActivityLifecycle::Started {
                token,
                activity: GlobalBindingActivity(binding),
            } => {
                let token = *token;
                let binding = Arc::clone(binding);
                commands.queue(move |world: &mut World| {
                    let Ok(mut entity) = world.get_entity_mut(token.entity()) else {
                        return;
                    };
                    if entity.contains::<GlobalBindingListeners<B>>() {
                        entity
                            .get_mut::<GlobalBindingListeners<B>>()
                            .expect("checked binding listener component exists")
                            .runs
                            .push(GlobalBindingRun {
                                token,
                                binding,
                                needs_initial: true,
                            });
                    } else {
                        entity.insert(GlobalBindingListeners::<B> {
                            runs: vec![GlobalBindingRun {
                                token,
                                binding,
                                needs_initial: true,
                            }],
                        });
                    }
                });
            }
            StateTreeActivityLifecycle::Cancelled { token } => {
                let token = *token;
                commands.queue(move |world: &mut World| {
                    let Ok(mut entity) = world.get_entity_mut(token.entity()) else {
                        return;
                    };
                    let empty = {
                        let Some(mut listeners) = entity.get_mut::<GlobalBindingListeners<B>>()
                        else {
                            return;
                        };
                        listeners.runs.retain(|run| run.token != token);
                        listeners.runs.is_empty()
                    };
                    if empty {
                        entity.remove::<GlobalBindingListeners<B>>();
                    }
                });
            }
        }
    }
}

fn prune_global_binding<B>(mut commands: Commands, mut listeners: PrunableGlobalBindings<B>)
where
    B: StateTreeBinding,
{
    for (entity, instance, mut listeners) in &mut listeners {
        listeners.runs.retain(|run| {
            instance.is_some_and(|instance| instance.accepts_activity_token(run.token.erased()))
        });
        if listeners.runs.is_empty() {
            commands
                .entity(entity)
                .remove::<GlobalBindingListeners<B>>();
        }
    }
}

fn dispatch_global_binding<B>(
    In(value): In<B::Value>,
    mut listeners: Query<(Entity, &StateTreeInstance, &mut GlobalBindingListeners<B>)>,
    mut previous: Local<Option<B::Value>>,
    mut events: EntityStateTreeEventWriter,
) where
    B: StateTreeBinding,
{
    let changed = previous.as_ref() != Some(&value);
    for (entity, instance, mut listeners) in &mut listeners {
        for run in &mut listeners.runs {
            if !instance.accepts_activity_token(run.token.erased())
                || (!changed && !run.needs_initial)
            {
                continue;
            }
            if events.write(run.binding.event(entity, &value)) {
                run.needs_initial = false;
            }
        }
    }
    *previous = Some(value);
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use bevy_app::{App, Update};
    use bevy_asset::{AssetPlugin, Assets};
    use bevy_ecs::{
        event::EntityEvent,
        prelude::{Entity, ResMut, Resource},
        world::World,
    };
    use bevy_time::TimePlugin;

    use super::*;
    use crate::{
        LuauValue, StateTreeDefinition, StateTreeEvent, StateTreeEventDataError, StateTreePlugin,
        scxml,
    };

    fn configure_binding(config: &mut crate::StateTreeConfig<'_>) {
        config
            .global_binding(
                "bind.sample",
                |_context, _arguments| Ok(SampleBinding),
                sample_value,
            )
            .event::<SampleChanged>();
    }

    #[derive(Clone)]
    #[cfg_attr(feature = "persistence", derive(serde::Serialize, serde::Deserialize))]
    struct SampleBinding;

    impl StateTreeBinding for SampleBinding {
        type Value = SampleValue;
        type Event = SampleChanged;

        fn event(&self, entity: Entity, value: &Self::Value) -> Self::Event {
            SampleChanged {
                entity,
                value: value.clone(),
            }
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    #[cfg_attr(feature = "persistence", derive(serde::Serialize, serde::Deserialize))]
    enum SampleMode {
        Idle,
        Busy,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    #[cfg_attr(feature = "persistence", derive(serde::Serialize, serde::Deserialize))]
    struct SampleValue {
        mode: SampleMode,
        label: String,
    }

    #[derive(Resource)]
    #[cfg_attr(feature = "persistence", derive(serde::Serialize, serde::Deserialize))]
    struct SampleSource {
        value: SampleValue,
        reads: usize,
    }

    impl Default for SampleSource {
        fn default() -> Self {
            Self {
                value: SampleValue {
                    mode: SampleMode::Idle,
                    label: "first".into(),
                },
                reads: 0,
            }
        }
    }

    fn sample_value(mut source: ResMut<SampleSource>) -> SampleValue {
        source.reads += 1;
        source.value.clone()
    }

    #[derive(Clone, EntityEvent, StateTreeEvent)]
    #[state_tree(
        name = "sample.changed",
        data = project_sample
    )]
    struct SampleChanged {
        entity: Entity,
        value: SampleValue,
    }

    fn project_sample(event: &SampleChanged) -> Result<LuauValue, StateTreeEventDataError> {
        Ok(LuauValue::Record(BTreeMap::from([
            (
                "mode".into(),
                LuauValue::String(
                    match event.value.mode {
                        SampleMode::Idle => "idle",
                        SampleMode::Busy => "busy",
                    }
                    .into(),
                ),
            ),
            ("label".into(), LuauValue::String(event.value.label.clone())),
        ])))
    }

    #[test]
    fn global_binding_is_lazy_and_delivers_initial_and_semantic_changes() {
        let mut app = binding_app();
        app.update();
        assert_eq!(app.world().resource::<SampleSource>().reads, 0);

        let definition = binding_definition();
        let handle = app
            .world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .add(definition);
        let first = app
            .world_mut()
            .spawn(StateTreeInstance::new(handle.clone()))
            .id();

        app.update();
        assert_eq!(app.world().resource::<SampleSource>().reads, 0);
        app.update();
        assert_eq!(app.world().resource::<SampleSource>().reads, 1);
        assert_binding_data(app.world(), first, 1, "idle", "first");

        app.update();
        assert_eq!(app.world().resource::<SampleSource>().reads, 2);
        assert_binding_data(app.world(), first, 1, "idle", "first");

        app.world_mut().resource_mut::<SampleSource>().value = SampleValue {
            mode: SampleMode::Busy,
            label: "second".into(),
        };
        app.update();
        assert_eq!(app.world().resource::<SampleSource>().reads, 3);
        assert_binding_data(app.world(), first, 2, "busy", "second");

        let second = app
            .world_mut()
            .spawn(StateTreeInstance::new(handle.clone()))
            .id();
        app.update();
        assert_eq!(app.world().resource::<SampleSource>().reads, 4);
        app.update();
        assert_eq!(app.world().resource::<SampleSource>().reads, 5);
        assert_binding_data(app.world(), first, 2, "busy", "second");
        assert_binding_data(app.world(), second, 1, "busy", "second");

        app.world_mut()
            .entity_mut(first)
            .insert(StateTreeInstance::new(handle));
        app.update();
        assert_eq!(app.world().resource::<SampleSource>().reads, 6);
        app.update();
        assert_eq!(app.world().resource::<SampleSource>().reads, 7);
        assert_binding_data(app.world(), first, 1, "busy", "second");
        assert_binding_data(app.world(), second, 1, "busy", "second");

        app.world_mut()
            .entity_mut(first)
            .remove::<StateTreeInstance>();
        app.update();
        assert_eq!(app.world().resource::<SampleSource>().reads, 8);
        app.world_mut()
            .entity_mut(second)
            .remove::<StateTreeInstance>();
        app.update();
        assert_eq!(app.world().resource::<SampleSource>().reads, 8);
        app.update();
        assert_eq!(app.world().resource::<SampleSource>().reads, 8);
    }

    #[cfg(feature = "persistence")]
    #[test]
    fn binding_checkpoint_rebuilds_listener_and_delivers_a_fresh_initial_sample() {
        use crate::persistence::{AppPersistenceExt, Checkpoint};
        let mut source = binding_app();
        source.persist_resource::<SampleSource>("sample-source");
        let handle = source
            .world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .add(binding_definition());
        let original = source
            .world_mut()
            .spawn(StateTreeInstance::new(handle))
            .id();
        source.update();
        source.update();
        assert_binding_data(source.world(), original, 1, "idle", "first");
        let checkpoint = Checkpoint::capture(source.world_mut()).unwrap();
        let mut target = binding_app();
        target.persist_resource::<SampleSource>("sample-source");
        target
            .world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .add(binding_definition());
        checkpoint.restore(target.world_mut()).unwrap();
        let restored = target
            .world_mut()
            .query_filtered::<Entity, bevy_ecs::prelude::With<StateTreeInstance>>()
            .single(target.world())
            .unwrap();
        target.world_mut().run_schedule(Update);
        target.world_mut().run_schedule(Update);
        assert_binding_data(target.world(), restored, 2, "idle", "first");
        target.world_mut().resource_mut::<SampleSource>().value = SampleValue {
            mode: SampleMode::Busy,
            label: "next".into(),
        };
        target.world_mut().run_schedule(Update);
        assert_binding_data(target.world(), restored, 3, "busy", "next");
        assert_eq!(
            target
                .world()
                .get::<GlobalBindingListeners<SampleBinding>>(restored)
                .unwrap()
                .runs
                .len(),
            1
        );
    }

    fn binding_app() -> App {
        let mut app = App::new();
        app.add_plugins((
            TimePlugin,
            AssetPlugin::default(),
            StateTreePlugin::new(Update).configure(configure_binding),
        ));
        app.insert_resource(SampleSource {
            value: SampleValue {
                mode: SampleMode::Idle,
                label: "first".into(),
            },
            reads: 0,
        });
        app
    }

    fn binding_definition() -> StateTreeDefinition {
        let source = r#"
            <scxml xmlns="http://www.w3.org/2005/07/scxml"
                   xmlns:bst="urn:bevy-state-tree"
                   datamodel="luau"
                   initial="active">
              <datamodel>
                <data id="changes" expr="0"/>
                <data id="mode" expr="nil"/>
                <data id="label" expr="nil"/>
              </datamodel>
              <state id="active">
                <bst:activity id="sample" name="bind.sample"/>
                <transition event="sample.changed">
                  <assign location="changes" expr="changes + 1"/>
                  <assign location="mode" expr="_event.data.mode"/>
                  <assign location="label" expr="_event.data.label"/>
                </transition>
              </state>
            </scxml>
        "#;
        let document = scxml::parse("binding.scxml", source).unwrap();
        scxml::compile(&document).unwrap()
    }

    #[test]
    fn nested_assignments_persist_only_the_root_and_support_event_keys_and_deletion() {
        let source = r#"
            <scxml xmlns="http://www.w3.org/2005/07/scxml" xmlns:bst="urn:bevy-state-tree" datamodel="luau" initial="active">
              <datamodel>
                <data id="values" expr="{keep = {current = 5}, old = 1, list = {2, 3}}"/>
                <data id="key" expr="'unit'"/>
              </datamodel>
              <state id="active">
                <onentry>
                  <assign location="values[key]" expr="{current = 10}"/>
                  <assign location="values.list[2]" expr="7"/>
                  <assign location="values.old" expr="nil"/>
                </onentry>
                <bst:activity id="sample" name="bind.sample"/>
                <transition event="sample.changed">
                  <assign location="values[key].current" expr="values.keep.current + 1"/>
                  <assign location="values[_event.data.mode]" expr="_event.data.label"/>
                </transition>
              </state>
            </scxml>
        "#;
        let definition = scxml::compile(&scxml::parse("nested.scxml", source).unwrap()).unwrap();
        let mut app = binding_app();
        let handle = app
            .world_mut()
            .resource_mut::<Assets<StateTreeDefinition>>()
            .add(definition);
        let entity = app.world_mut().spawn(StateTreeInstance::new(handle)).id();
        app.update();
        app.update();
        let values = app
            .world()
            .get::<StateTreeInstance>(entity)
            .unwrap()
            .data("values")
            .unwrap();
        assert_eq!(
            values,
            &LuauValue::Record(BTreeMap::from([
                (
                    "keep".into(),
                    LuauValue::Record(BTreeMap::from([("current".into(), LuauValue::Integer(5))]))
                ),
                (
                    "unit".into(),
                    LuauValue::Record(BTreeMap::from([("current".into(), LuauValue::Integer(6))]))
                ),
                (
                    "list".into(),
                    LuauValue::Sequence(vec![LuauValue::Integer(2), LuauValue::Integer(7)])
                ),
                ("idle".into(), LuauValue::String("first".into())),
            ]))
        );
    }

    #[test]
    fn assignment_locations_reject_calls_multiple_targets_and_undeclared_roots() {
        for location in [
            "values()",
            "values().field",
            "values, other",
            "values; other",
            "missing.field",
        ] {
            let source = format!(
                r#"
                <scxml xmlns="http://www.w3.org/2005/07/scxml" xmlns:bst="urn:bevy-state-tree" datamodel="luau">
                  <datamodel><data id="values" expr="{{}}"/></datamodel>
                  <state id="active"><onentry><assign location="{location}" expr="1"/></onentry></state>
                </scxml>"#
            );
            let error =
                scxml::compile(&scxml::parse("invalid.scxml", &source).unwrap()).unwrap_err();
            if location == "missing.field" {
                assert!(
                    error
                        .to_string()
                        .contains("assignment location `missing` is not declared")
                );
            } else {
                assert!(
                    error.to_string().contains("invalid Luau"),
                    "location: {location}, error: {error}"
                );
            }
        }
    }

    #[test]
    fn invalid_nested_assignments_fault_without_persisting_partial_mutations() {
        for location in [
            "values.missing.current",
            "values.keep.current.field",
            "values[false]",
            "values.list[1]",
        ] {
            let expression = if location == "values.list[1]" {
                "nil"
            } else {
                "9"
            };
            let source = format!(
                r#"
                <scxml xmlns="http://www.w3.org/2005/07/scxml" xmlns:bst="urn:bevy-state-tree" datamodel="luau">
                  <datamodel><data id="values" expr="{{keep = {{current = 5}}, list = {{2, 3}}}}"/></datamodel>
                  <state id="active"><onentry><assign location="{location}" expr="{expression}"/></onentry></state>
                </scxml>"#
            );
            let definition =
                scxml::compile(&scxml::parse("invalid-nested.scxml", &source).unwrap()).unwrap();
            let mut app = binding_app();
            let handle = app
                .world_mut()
                .resource_mut::<Assets<StateTreeDefinition>>()
                .add(definition);
            let entity = app.world_mut().spawn(StateTreeInstance::new(handle)).id();
            app.update();
            let instance = app.world().get::<StateTreeInstance>(entity).unwrap();
            assert!(
                matches!(
                    instance.status(),
                    crate::InstanceStatus::Faulted(crate::RuntimeFault::Datamodel {
                        operation: crate::DatamodelOperation::Assign,
                        ..
                    })
                ),
                "{location}: {:?}",
                instance.status()
            );
            assert_eq!(
                instance.data("values"),
                Some(&LuauValue::Record(BTreeMap::from([
                    (
                        "keep".into(),
                        LuauValue::Record(BTreeMap::from([(
                            "current".into(),
                            LuauValue::Integer(5)
                        )]))
                    ),
                    (
                        "list".into(),
                        LuauValue::Sequence(vec![LuauValue::Integer(2), LuauValue::Integer(3)])
                    ),
                ])))
            );
        }
    }

    fn assert_binding_data(world: &World, entity: Entity, changes: i64, mode: &str, label: &str) {
        let instance = world
            .get::<StateTreeInstance>(entity)
            .expect("binding state tree exists");
        assert_eq!(instance.data("changes"), Some(&LuauValue::Integer(changes)));
        assert_eq!(instance.data("mode"), Some(&LuauValue::String(mode.into())));
        assert_eq!(
            instance.data("label"),
            Some(&LuauValue::String(label.into()))
        );
    }
}
