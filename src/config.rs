use bevy_app::App;
use bevy_ecs::message::Message;
use bevy_ecs::{schedule::InternedScheduleLabel, system::IntoSystem};

use crate::{
    StateTreeActivity, StateTreeActivityArgs, StateTreeActivityContext, StateTreeActivityError,
    StateTreeBinding, StateTreeEffectArgs, StateTreeEffectContext, StateTreeEffectError,
    StateTreeEvent,
};

/// Declares the application's typed events, effects, activities, and bindings.
///
/// Supplied during [`StateTreePlugin::configure`](crate::StateTreePlugin::configure).
/// Applications cannot mutate the protocol after plugin installation.
pub struct StateTreeConfig<'a> {
    app: &'a mut App,
    schedule: InternedScheduleLabel,
}

impl<'a> StateTreeConfig<'a> {
    pub(crate) fn new(app: &'a mut App, schedule: InternedScheduleLabel) -> Self {
        Self { app, schedule }
    }

    /// Declares one concrete external event payload.
    pub fn event<E>(&mut self) -> &mut Self
    where
        E: StateTreeEvent,
    {
        crate::event::register_event::<E>(
            self.app,
            crate::protocol::RegistrationOrigin::Application,
        );
        self
    }

    /// Declares one authored effect and its concrete Bevy message constructor.
    pub fn effect<M, F>(&mut self, name: impl Into<Box<str>>, constructor: F) -> &mut Self
    where
        M: Message,
        F: Fn(
                StateTreeEffectContext<'_>,
                &mut StateTreeEffectArgs,
            ) -> Result<M, StateTreeEffectError>
            + Send
            + Sync
            + 'static,
    {
        crate::effect::register_effect::<M, F>(
            self.app,
            name.into(),
            constructor,
            crate::protocol::RegistrationOrigin::Application,
        );
        self
    }

    /// Declares one authored activity and its concrete typed constructor.
    pub fn activity<A, F>(&mut self, name: impl Into<Box<str>>, constructor: F) -> &mut Self
    where
        A: StateTreeActivity,
        F: Fn(
                StateTreeActivityContext<'_>,
                &mut StateTreeActivityArgs,
            ) -> Result<A, StateTreeActivityError>
            + Send
            + Sync
            + 'static,
    {
        crate::activity::register_activity::<A, F>(
            self.app,
            name.into(),
            constructor,
            crate::protocol::RegistrationOrigin::Application,
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
        B: StateTreeBinding,
        F: Fn(
                StateTreeActivityContext<'_>,
                &mut StateTreeActivityArgs,
            ) -> Result<B, StateTreeActivityError>
            + Send
            + Sync
            + 'static,
        S: IntoSystem<(), B::Value, M> + 'static,
    {
        crate::binding::register_global_binding::<B, F, S, M>(
            self.app,
            self.schedule,
            name.into(),
            constructor,
            source,
        );
        self
    }
}
