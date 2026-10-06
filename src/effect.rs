use std::{collections::HashMap, marker::PhantomData, sync::Arc};

use bevy_app::App;
use bevy_ecs::{entity::Entity, message::Message, prelude::Resource, system::ParallelCommands};
use thiserror::Error;

use crate::{
    LuauValue, StateTreeDomain,
    event::{StateTreeEvent, StateTreeEventRef},
    protocol::RegistrationOrigin,
};

/// An error while constructing a typed state-tree effect.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("{message}")]
pub struct StateTreeEffectError {
    message: Box<str>,
    argument: Option<Box<str>>,
}

impl StateTreeEffectError {
    pub fn new(message: impl Into<Box<str>>) -> Self {
        Self {
            message: message.into(),
            argument: None,
        }
    }

    pub fn argument(name: impl Into<Box<str>>, message: impl Into<Box<str>>) -> Self {
        Self {
            message: message.into(),
            argument: Some(name.into()),
        }
    }

    pub fn argument_name(&self) -> Option<&str> {
        self.argument.as_deref()
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

/// Immutable execution context supplied to a typed state-tree constructor.
pub struct StateTreeContext<'a, D: StateTreeDomain> {
    pub(crate) entity: Entity,
    pub(crate) event_name: Option<&'a str>,
    pub(crate) external_event: Option<StateTreeEventRef<'a, D>>,
    pub(crate) data_names: &'a [Box<str>],
    pub(crate) data: &'a [LuauValue],
}

impl<'a, D: StateTreeDomain> StateTreeContext<'a, D> {
    /// Returns the Bevy entity running the tree.
    pub fn entity(&self) -> Entity {
        self.entity
    }

    /// Returns the current authored event name.
    pub fn event_name(&self) -> Option<&str> {
        self.event_name
    }

    /// Returns the current external event, if this action is executing in its context.
    pub fn external_event(&self) -> Option<StateTreeEventRef<'a, D>> {
        self.external_event
    }

    /// Borrows the current external event when it contains `T`.
    pub fn event<T: StateTreeEvent<D>>(&self) -> Option<&T> {
        self.external_event
            .and_then(StateTreeEventRef::payload::<T>)
    }

    /// Returns one instance-local datamodel value.
    pub fn data(&self, name: &str) -> Option<&LuauValue> {
        self.data_names
            .iter()
            .position(|candidate| candidate.as_ref() == name)
            .and_then(|index| self.data.get(index))
    }
}

/// Effect-specific alias for the shared immutable constructor context.
pub type StateTreeEffectContext<'a, D> = StateTreeContext<'a, D>;
/// Activity-specific alias for the shared immutable constructor context.
pub type StateTreeActivityContext<'a, D> = StateTreeContext<'a, D>;

/// Evaluated named arguments supplied to one typed state-tree constructor.
#[derive(Debug, Default)]
pub struct StateTreeArgs {
    values: Vec<(Arc<str>, LuauValue)>,
}

impl StateTreeArgs {
    pub(crate) fn new(values: Vec<(Arc<str>, LuauValue)>) -> Self {
        Self { values }
    }

    /// Takes and converts a required argument.
    pub fn take<T: FromStateTreeValue>(&mut self, name: &str) -> Result<T, StateTreeEffectError> {
        let Some(index) = self
            .values
            .iter()
            .position(|(candidate, _)| candidate.as_ref() == name)
        else {
            return Err(StateTreeEffectError::argument(
                name,
                "required state-tree argument is missing",
            ));
        };
        let (_, value) = self.values.remove(index);
        T::from_state_tree_value(value)
            .map_err(|message| StateTreeEffectError::argument(name, message))
    }

    /// Takes and converts an argument, returning `default` when it is absent.
    pub fn take_or<T: FromStateTreeValue>(
        &mut self,
        name: &str,
        default: T,
    ) -> Result<T, StateTreeEffectError> {
        if self
            .values
            .iter()
            .any(|(candidate, _)| candidate.as_ref() == name)
        {
            self.take(name)
        } else {
            Ok(default)
        }
    }

    pub(crate) fn finish(self) -> Result<(), StateTreeEffectError> {
        if let Some((name, _)) = self.values.first() {
            return Err(StateTreeEffectError::argument(
                Box::<str>::from(name.as_ref()),
                "state-tree argument was not consumed by its constructor",
            ));
        }
        Ok(())
    }

    /// Iterates over arguments that have not been consumed.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&str, &LuauValue)> {
        self.values
            .iter()
            .map(|(name, value)| (name.as_ref(), value))
    }
}

/// Effect-specific name for [`StateTreeArgs`].
pub type StateTreeEffectArgs = StateTreeArgs;
/// Activity-specific name for [`StateTreeArgs`].
pub type StateTreeActivityArgs = StateTreeArgs;
/// Activity constructors use the shared effect-construction error type.
pub type StateTreeActivityError = StateTreeEffectError;

/// Converts a portable Luau value into a Rust effect argument.
pub trait FromStateTreeValue: Sized {
    fn from_state_tree_value(value: LuauValue) -> Result<Self, Box<str>>;
}

impl FromStateTreeValue for LuauValue {
    fn from_state_tree_value(value: LuauValue) -> Result<Self, Box<str>> {
        Ok(value)
    }
}

impl FromStateTreeValue for bool {
    fn from_state_tree_value(value: LuauValue) -> Result<Self, Box<str>> {
        match value {
            LuauValue::Boolean(value) => Ok(value),
            _ => Err("expected a boolean".into()),
        }
    }
}

impl FromStateTreeValue for String {
    fn from_state_tree_value(value: LuauValue) -> Result<Self, Box<str>> {
        match value {
            LuauValue::String(value) => Ok(value.into()),
            _ => Err("expected a string".into()),
        }
    }
}

impl FromStateTreeValue for i64 {
    fn from_state_tree_value(value: LuauValue) -> Result<Self, Box<str>> {
        match value {
            LuauValue::Integer(value) => Ok(value),
            _ => Err("expected an integer".into()),
        }
    }
}

impl FromStateTreeValue for u32 {
    fn from_state_tree_value(value: LuauValue) -> Result<Self, Box<str>> {
        match value {
            LuauValue::Integer(value) => {
                u32::try_from(value).map_err(|_| "integer is outside the u32 range".into())
            }
            _ => Err("expected an integer".into()),
        }
    }
}

impl FromStateTreeValue for f64 {
    fn from_state_tree_value(value: LuauValue) -> Result<Self, Box<str>> {
        match value {
            LuauValue::Integer(value) => Ok(value as f64),
            LuauValue::Number(value) => Ok(value),
            _ => Err("expected a number".into()),
        }
    }
}

impl FromStateTreeValue for f32 {
    fn from_state_tree_value(value: LuauValue) -> Result<Self, Box<str>> {
        let value = f64::from_state_tree_value(value)?;
        if value.is_finite() && value >= f32::MIN as f64 && value <= f32::MAX as f64 {
            Ok(value as f32)
        } else {
            Err("number is outside the finite f32 range".into())
        }
    }
}

pub(crate) trait RegisteredEffect<D: StateTreeDomain>: Send + Sync {
    fn publish(
        &self,
        context: StateTreeEffectContext<'_, D>,
        arguments: StateTreeEffectArgs,
        commands: &ParallelCommands<'_, '_>,
    ) -> Result<(), StateTreeEffectError>;
}

struct MessageEffect<D, M, F> {
    constructor: F,
    marker: PhantomData<fn() -> (D, M)>,
}

impl<D, M, F> RegisteredEffect<D> for MessageEffect<D, M, F>
where
    D: StateTreeDomain,
    M: Message,
    F: Fn(
            StateTreeEffectContext<'_, D>,
            &mut StateTreeEffectArgs,
        ) -> Result<M, StateTreeEffectError>
        + Send
        + Sync
        + 'static,
{
    fn publish(
        &self,
        context: StateTreeEffectContext<'_, D>,
        mut arguments: StateTreeEffectArgs,
        commands: &ParallelCommands<'_, '_>,
    ) -> Result<(), StateTreeEffectError> {
        let message = (self.constructor)(context, &mut arguments)?;
        arguments.finish()?;
        commands.command_scope(move |mut commands| {
            commands.write_message(message);
        });
        Ok(())
    }
}

#[derive(Resource)]
pub(crate) struct StateTreeEffectRegistry<D: StateTreeDomain> {
    effects: HashMap<Box<str>, Arc<dyn RegisteredEffect<D>>>,
    origins: HashMap<Box<str>, RegistrationOrigin>,
}

impl<D: StateTreeDomain> Default for StateTreeEffectRegistry<D> {
    fn default() -> Self {
        Self {
            effects: HashMap::new(),
            origins: HashMap::new(),
        }
    }
}

impl<D: StateTreeDomain> StateTreeEffectRegistry<D> {
    pub(crate) fn get(&self, name: &str) -> Option<Arc<dyn RegisteredEffect<D>>> {
        self.effects.get(name).cloned()
    }

    pub(crate) fn registrations(&self) -> impl Iterator<Item = (&str, RegistrationOrigin)> {
        self.origins
            .iter()
            .map(|(name, origin)| (name.as_ref(), *origin))
    }
}

pub(crate) trait RuntimeEffectSink<D: StateTreeDomain> {
    fn emit(
        &mut self,
        handler: &Arc<dyn RegisteredEffect<D>>,
        context: StateTreeEffectContext<'_, D>,
        arguments: StateTreeEffectArgs,
    ) -> Result<(), StateTreeEffectError>;
}

pub(crate) struct BevyEffectSink<'a, 'w, 's> {
    pub commands: &'a ParallelCommands<'w, 's>,
}

impl<D: StateTreeDomain> RuntimeEffectSink<D> for BevyEffectSink<'_, '_, '_> {
    fn emit(
        &mut self,
        handler: &Arc<dyn RegisteredEffect<D>>,
        context: StateTreeEffectContext<'_, D>,
        arguments: StateTreeEffectArgs,
    ) -> Result<(), StateTreeEffectError> {
        handler.publish(context, arguments, self.commands)
    }
}

pub(crate) fn register_effect<D, M, F>(
    app: &mut App,
    name: Box<str>,
    constructor: F,
    origin: RegistrationOrigin,
) where
    D: StateTreeDomain,
    M: Message,
    F: Fn(
            StateTreeEffectContext<'_, D>,
            &mut StateTreeEffectArgs,
        ) -> Result<M, StateTreeEffectError>
        + Send
        + Sync
        + 'static,
{
    assert!(!name.is_empty(), "state-tree effect names cannot be empty");
    app.add_message::<M>();
    let registry = app
        .world_mut()
        .resource_mut::<StateTreeEffectRegistry<D>>()
        .into_inner();
    assert!(
        !registry.effects.contains_key(&name),
        "state-tree effect `{name}` was registered more than once"
    );
    registry.effects.insert(
        name.clone(),
        Arc::new(MessageEffect::<D, M, F> {
            constructor,
            marker: PhantomData,
        }),
    );
    registry.origins.insert(name, origin);
}

#[cfg(test)]
mod tests {
    use bevy_ecs::message::Message;

    use super::*;

    struct TestDomain;

    impl StateTreeDomain for TestDomain {
        const AUTHORED_NAME: &'static str = "test";

        fn configure(_config: &mut crate::StateTreeConfig<'_, Self>) {}
    }

    #[derive(Message)]
    struct TestEffect;

    #[test]
    #[should_panic(expected = "state-tree effect `duplicate` was registered more than once")]
    fn effect_names_must_be_unique_within_a_domain() {
        let mut app = App::new();
        app.init_resource::<StateTreeEffectRegistry<TestDomain>>();
        register_effect::<TestDomain, TestEffect, _>(
            &mut app,
            "duplicate".into(),
            |_context, _arguments| Ok(TestEffect),
            RegistrationOrigin::Domain,
        );
        register_effect::<TestDomain, TestEffect, _>(
            &mut app,
            "duplicate".into(),
            |_context, _arguments| Ok(TestEffect),
            RegistrationOrigin::Domain,
        );
    }
}
