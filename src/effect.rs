use std::{collections::HashMap, marker::PhantomData, sync::Arc};

use bevy_app::App;
use bevy_ecs::{entity::Entity, message::Message, prelude::Resource, system::ParallelCommands};
use thiserror::Error;

use crate::{LuauValue, StateTreeDomain, StateTreeEvents};

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

/// Immutable execution context supplied to one effect constructor.
pub struct StateTreeEffectContext<'a, D: StateTreeDomain> {
    pub(crate) entity: Option<Entity>,
    pub(crate) event_name: Option<&'a str>,
    pub(crate) external_event: Option<&'a D::Event>,
    pub(crate) data_names: &'a [Box<str>],
    pub(crate) data: &'a [LuauValue],
}

impl<'a, D: StateTreeDomain> StateTreeEffectContext<'a, D> {
    /// Returns the Bevy entity running the tree, or `None` for headless execution.
    pub fn entity(&self) -> Option<Entity> {
        self.entity
    }

    /// Returns the current authored event name.
    pub fn event_name(&self) -> Option<&str> {
        self.event_name
    }

    /// Returns the current external event, if this action is executing in its context.
    pub fn external_event(&self) -> Option<&D::Event> {
        self.external_event
    }

    /// Returns one instance-local datamodel value.
    pub fn data(&self, name: &str) -> Option<&LuauValue> {
        self.data_names
            .iter()
            .position(|candidate| candidate.as_ref() == name)
            .and_then(|index| self.data.get(index))
    }
}

/// Evaluated named arguments supplied to one effect constructor.
#[derive(Debug, Default)]
pub struct StateTreeEffectArgs {
    values: Vec<(Box<str>, LuauValue)>,
}

impl StateTreeEffectArgs {
    pub(crate) fn new(values: Vec<(Box<str>, LuauValue)>) -> Self {
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
                "required effect argument is missing",
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
                name.clone(),
                "effect argument was not consumed by its constructor",
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

/// One portable effect request emitted by headless execution.
pub struct StateTreeEffectRequest<'a, E: StateTreeEvents> {
    name: &'a str,
    span_start: usize,
    event_name: Option<&'a str>,
    external_event: Option<&'a E>,
    data_names: &'a [Box<str>],
    data: &'a [LuauValue],
    arguments: &'a mut StateTreeEffectArgs,
}

impl<'a, E: StateTreeEvents> StateTreeEffectRequest<'a, E> {
    pub fn name(&self) -> &str {
        self.name
    }

    pub fn span_start(&self) -> usize {
        self.span_start
    }

    pub fn event_name(&self) -> Option<&str> {
        self.event_name
    }

    pub fn external_event(&self) -> Option<&E> {
        self.external_event
    }

    pub fn data(&self, name: &str) -> Option<&LuauValue> {
        self.data_names
            .iter()
            .position(|candidate| candidate.as_ref() == name)
            .and_then(|index| self.data.get(index))
    }

    pub fn arguments(&mut self) -> &mut StateTreeEffectArgs {
        self.arguments
    }
}

/// Receives effects from a headless state-tree execution.
pub trait StateTreeEffectSink<E: StateTreeEvents> {
    fn emit(&mut self, request: StateTreeEffectRequest<'_, E>) -> Result<(), StateTreeEffectError>;
}

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
}

impl<D: StateTreeDomain> Default for StateTreeEffectRegistry<D> {
    fn default() -> Self {
        Self {
            effects: HashMap::new(),
        }
    }
}

impl<D: StateTreeDomain> StateTreeEffectRegistry<D> {
    pub(crate) fn get(&self, name: &str) -> Option<Arc<dyn RegisteredEffect<D>>> {
        self.effects.get(name).cloned()
    }
}

pub(crate) trait RuntimeEffectSink<D: StateTreeDomain> {
    fn emit(
        &mut self,
        name: &str,
        handler: Option<&Arc<dyn RegisteredEffect<D>>>,
        context: StateTreeEffectContext<'_, D>,
        arguments: StateTreeEffectArgs,
        span_start: usize,
    ) -> Result<(), StateTreeEffectError>;
}

pub(crate) struct RejectEffects;

impl<D: StateTreeDomain> RuntimeEffectSink<D> for RejectEffects {
    fn emit(
        &mut self,
        name: &str,
        _handler: Option<&Arc<dyn RegisteredEffect<D>>>,
        _context: StateTreeEffectContext<'_, D>,
        _arguments: StateTreeEffectArgs,
        _span_start: usize,
    ) -> Result<(), StateTreeEffectError> {
        Err(StateTreeEffectError::new(format!(
            "effect `{name}` requires an effect sink"
        )))
    }
}

pub(crate) struct BevyEffectSink<'a, 'w, 's> {
    pub commands: &'a ParallelCommands<'w, 's>,
}

impl<D: StateTreeDomain> RuntimeEffectSink<D> for BevyEffectSink<'_, '_, '_> {
    fn emit(
        &mut self,
        name: &str,
        handler: Option<&Arc<dyn RegisteredEffect<D>>>,
        context: StateTreeEffectContext<'_, D>,
        arguments: StateTreeEffectArgs,
        _span_start: usize,
    ) -> Result<(), StateTreeEffectError> {
        let Some(handler) = handler else {
            return Err(StateTreeEffectError::new(format!(
                "effect `{name}` is not registered for this domain"
            )));
        };
        handler.publish(context, arguments, self.commands)
    }
}

pub(crate) struct HeadlessEffectSink<'a, S: ?Sized> {
    pub sink: &'a mut S,
}

impl<D, S> RuntimeEffectSink<D> for HeadlessEffectSink<'_, S>
where
    D: StateTreeDomain,
    S: StateTreeEffectSink<D::Event> + ?Sized,
{
    fn emit(
        &mut self,
        name: &str,
        _handler: Option<&Arc<dyn RegisteredEffect<D>>>,
        context: StateTreeEffectContext<'_, D>,
        mut arguments: StateTreeEffectArgs,
        span_start: usize,
    ) -> Result<(), StateTreeEffectError> {
        self.sink.emit(StateTreeEffectRequest {
            name,
            span_start,
            event_name: context.event_name,
            external_event: context.external_event,
            data_names: context.data_names,
            data: context.data,
            arguments: &mut arguments,
        })
    }
}

/// Registers typed state-tree effects on a Bevy application.
pub trait StateTreeAppExt {
    fn register_state_tree_effect<D, M, F>(
        &mut self,
        name: impl Into<Box<str>>,
        constructor: F,
    ) -> &mut Self
    where
        D: StateTreeDomain,
        M: Message,
        F: Fn(
                StateTreeEffectContext<'_, D>,
                &mut StateTreeEffectArgs,
            ) -> Result<M, StateTreeEffectError>
            + Send
            + Sync
            + 'static;
}

impl StateTreeAppExt for App {
    fn register_state_tree_effect<D, M, F>(
        &mut self,
        name: impl Into<Box<str>>,
        constructor: F,
    ) -> &mut Self
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
        let name = name.into();
        assert!(!name.is_empty(), "state-tree effect names cannot be empty");
        self.add_message::<M>();
        self.init_resource::<StateTreeEffectRegistry<D>>();
        let registry = self
            .world_mut()
            .resource_mut::<StateTreeEffectRegistry<D>>()
            .into_inner();
        let previous = registry.effects.insert(
            name.clone(),
            Arc::new(MessageEffect::<D, M, F> {
                constructor,
                marker: PhantomData,
            }),
        );
        assert!(
            previous.is_none(),
            "state-tree effect `{name}` was registered more than once"
        );
        self
    }
}
