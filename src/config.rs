use std::marker::PhantomData;

use bevy_app::App;
use bevy_ecs::message::Message;

use crate::{
    StateTreeActivity, StateTreeActivityArgs, StateTreeActivityContext, StateTreeActivityError,
    StateTreeDomain, StateTreeEffectArgs, StateTreeEffectContext, StateTreeEffectError,
    StateTreeEvent,
};

/// Declares every typed event, effect, and activity in one state-tree domain.
///
/// A domain receives this registrar from [`StateTreePlugin`](crate::StateTreePlugin)
/// through [`StateTreeDomain::configure`]. Applications cannot construct it or
/// mutate a domain's protocol through another route.
pub struct StateTreeConfig<'a, D: StateTreeDomain> {
    app: &'a mut App,
    marker: PhantomData<fn() -> D>,
}

impl<'a, D: StateTreeDomain> StateTreeConfig<'a, D> {
    pub(crate) fn new(app: &'a mut App) -> Self {
        Self {
            app,
            marker: PhantomData,
        }
    }

    /// Declares one concrete external event payload.
    pub fn event<E>(&mut self) -> &mut Self
    where
        E: StateTreeEvent<D>,
    {
        crate::event::register_event::<D, E>(self.app);
        self
    }

    /// Declares one authored effect and its concrete Bevy message constructor.
    pub fn effect<M, F>(&mut self, name: impl Into<Box<str>>, constructor: F) -> &mut Self
    where
        M: Message,
        F: Fn(
                StateTreeEffectContext<'_, D>,
                &mut StateTreeEffectArgs,
            ) -> Result<M, StateTreeEffectError>
            + Send
            + Sync
            + 'static,
    {
        crate::effect::register_effect::<D, M, F>(self.app, name.into(), constructor);
        self
    }

    /// Declares one authored activity and its concrete typed constructor.
    pub fn activity<A, F>(&mut self, name: impl Into<Box<str>>, constructor: F) -> &mut Self
    where
        A: StateTreeActivity,
        F: Fn(
                StateTreeActivityContext<'_, D>,
                &mut StateTreeActivityArgs,
            ) -> Result<A, StateTreeActivityError>
            + Send
            + Sync
            + 'static,
    {
        crate::activity::register_activity::<D, A, F>(self.app, name.into(), constructor);
        self
    }
}
