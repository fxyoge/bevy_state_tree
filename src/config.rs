use std::marker::PhantomData;

use bevy_app::App;
use bevy_ecs::message::Message;
use bevy_ecs::{schedule::InternedScheduleLabel, system::IntoSystem};

use crate::{
    StateTreeActivity, StateTreeActivityArgs, StateTreeActivityContext, StateTreeActivityError,
    StateTreeBinding, StateTreeDomain, StateTreeEffectArgs, StateTreeEffectContext,
    StateTreeEffectError, StateTreeEvent,
};

/// Declares every typed event, effect, and activity in one state-tree domain.
///
/// A domain receives this registrar from [`StateTreePlugin`](crate::StateTreePlugin)
/// through [`StateTreeDomain::configure`]. Applications cannot construct it or
/// mutate a domain's protocol through another route.
pub struct StateTreeConfig<'a, D: StateTreeDomain> {
    app: &'a mut App,
    schedule: InternedScheduleLabel,
    marker: PhantomData<fn() -> D>,
}

impl<'a, D: StateTreeDomain> StateTreeConfig<'a, D> {
    pub(crate) fn new(app: &'a mut App, schedule: InternedScheduleLabel) -> Self {
        Self {
            app,
            schedule,
            marker: PhantomData,
        }
    }

    /// Declares one concrete external event payload.
    pub fn event<E>(&mut self) -> &mut Self
    where
        E: StateTreeEvent<D>,
    {
        crate::event::register_event::<D, E>(self.app, crate::protocol::RegistrationOrigin::Domain);
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
        crate::effect::register_effect::<D, M, F>(
            self.app,
            name.into(),
            constructor,
            crate::protocol::RegistrationOrigin::Domain,
        );
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
        crate::activity::register_activity::<D, A, F>(
            self.app,
            name.into(),
            constructor,
            crate::protocol::RegistrationOrigin::Domain,
        );
        self
    }

    /// Declares one lazily sampled global value binding.
    ///
    /// `source` is an ordinary no-input Bevy system and is not run when no
    /// active activity listens to this binding. Each new listener receives the
    /// current value once; existing listeners receive another event only when
    /// the returned value changes according to [`PartialEq`].
    pub fn global_binding<B, F, S, M>(
        &mut self,
        name: impl Into<Box<str>>,
        constructor: F,
        source: S,
    ) -> &mut Self
    where
        B: StateTreeBinding<D>,
        F: Fn(
                StateTreeActivityContext<'_, D>,
                &mut StateTreeActivityArgs,
            ) -> Result<B, StateTreeActivityError>
            + Send
            + Sync
            + 'static,
        S: IntoSystem<(), B::Value, M> + 'static,
    {
        crate::binding::register_global_binding::<D, B, F, S, M>(
            self.app,
            self.schedule,
            name.into(),
            constructor,
            source,
        );
        self
    }
}
