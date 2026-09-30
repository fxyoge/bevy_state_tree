use std::fmt::Write;

use crate::{InstanceStatus, LuauValue, RuntimeFault};

/// Receives an ordered semantic trace from one state-tree execution.
///
/// Records describe deterministic semantic execution, including operations
/// that begin before a runtime fault. They do not expose worker threads, Bevy
/// schedules, speculative transition candidates, or other implementation
/// details. A sink must not attempt to re-enter the execution that is currently
/// producing records.
pub trait StateTreeTrace {
    /// Observes one record in execution order.
    fn record(&mut self, record: StateTreeTraceRecord<'_>);
}

/// A trace sink that discards every record.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoStateTreeTrace;

impl StateTreeTrace for NoStateTreeTrace {
    #[inline]
    fn record(&mut self, _record: StateTreeTraceRecord<'_>) {}
}

/// The event that initiated a macrostep.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StateTreeTraceTrigger<'a> {
    Initialization,
    ExternalEvent(&'a str),
    Timer,
    ActivityOutcome(&'a str),
}

/// One semantic operation in a state-tree execution.
///
/// A microstep begins with [`Self::MicrostepStarted`], followed by every
/// selected transition in canonical order. Multiple transitions may belong to
/// the same microstep when orthogonal regions are supported.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum StateTreeTraceRecord<'a> {
    MacrostepStarted {
        trigger: StateTreeTraceTrigger<'a>,
    },
    MicrostepStarted {
        index: usize,
    },
    TransitionSelected {
        source: &'a str,
    },
    TransitionTarget {
        state: &'a str,
    },
    TransitionTargetless,
    StateExitStarted {
        state: &'a str,
    },
    StateExited {
        state: &'a str,
    },
    StateEntered {
        state: &'a str,
    },
    InternalEventRaised {
        event: &'a str,
    },
    InternalEventConsumed {
        event: &'a str,
    },
    DataAssigned {
        data: &'a str,
        value: &'a LuauValue,
    },
    EffectEmitted {
        effect: &'a str,
    },
    ActivityStarted {
        activity: &'a str,
        generation: u64,
    },
    ActivityCancelled {
        activity: &'a str,
        generation: u64,
    },
    ActivityOutcome {
        activity: &'a str,
        success: bool,
        generation: u64,
    },
    ConfigurationStarted,
    ActiveState {
        state: &'a str,
    },
    Status {
        status: &'a InstanceStatus,
    },
    MacrostepEnded,
}

/// A stable, human-readable semantic event log.
///
/// The format is intended for checked-in golden traces and starts with a
/// version header so incompatible changes are explicit.
#[derive(Clone, Debug)]
pub struct StateTreeEventLog {
    output: String,
}

impl StateTreeEventLog {
    pub const FORMAT_VERSION: u32 = 1;

    /// Creates an empty versioned event log.
    pub fn new() -> Self {
        Self {
            output: format!("state-tree-trace {}\n", Self::FORMAT_VERSION),
        }
    }

    /// Returns the formatted trace.
    pub fn as_str(&self) -> &str {
        &self.output
    }

    /// Consumes the log and returns its formatted trace.
    pub fn into_string(self) -> String {
        self.output
    }
}

impl Default for StateTreeEventLog {
    fn default() -> Self {
        Self::new()
    }
}

impl StateTreeTrace for StateTreeEventLog {
    fn record(&mut self, record: StateTreeTraceRecord<'_>) {
        match record {
            StateTreeTraceRecord::MacrostepStarted {
                trigger: StateTreeTraceTrigger::Initialization,
            } => self.output.push_str("macrostep initialize\n"),
            StateTreeTraceRecord::MacrostepStarted {
                trigger: StateTreeTraceTrigger::ExternalEvent(event),
            } => {
                writeln!(self.output, "macrostep external {event:?}").unwrap();
            }
            StateTreeTraceRecord::MacrostepStarted {
                trigger: StateTreeTraceTrigger::Timer,
            } => self.output.push_str("macrostep timer\n"),
            StateTreeTraceRecord::MacrostepStarted {
                trigger: StateTreeTraceTrigger::ActivityOutcome(activity),
            } => writeln!(self.output, "macrostep activity {activity:?}").unwrap(),
            StateTreeTraceRecord::MicrostepStarted { index } => {
                writeln!(self.output, "microstep {index}").unwrap();
            }
            StateTreeTraceRecord::TransitionSelected { source } => {
                writeln!(self.output, "transition {source:?}").unwrap();
            }
            StateTreeTraceRecord::TransitionTarget { state } => {
                writeln!(self.output, "target {state:?}").unwrap();
            }
            StateTreeTraceRecord::TransitionTargetless => {
                self.output.push_str("targetless\n");
            }
            StateTreeTraceRecord::StateExitStarted { state } => {
                writeln!(self.output, "exit begin {state:?}").unwrap();
            }
            StateTreeTraceRecord::StateExited { state } => {
                writeln!(self.output, "exit {state:?}").unwrap();
            }
            StateTreeTraceRecord::StateEntered { state } => {
                writeln!(self.output, "enter {state:?}").unwrap();
            }
            StateTreeTraceRecord::InternalEventRaised { event } => {
                writeln!(self.output, "raise {event:?}").unwrap();
            }
            StateTreeTraceRecord::InternalEventConsumed { event } => {
                writeln!(self.output, "consume internal {event:?}").unwrap();
            }
            StateTreeTraceRecord::DataAssigned { data, value } => {
                write!(self.output, "assign {data:?} = ").unwrap();
                write_luau_value(&mut self.output, value);
                self.output.push('\n');
            }
            StateTreeTraceRecord::EffectEmitted { effect } => {
                writeln!(self.output, "effect {effect:?}").unwrap();
            }
            StateTreeTraceRecord::ActivityStarted {
                activity,
                generation,
            } => writeln!(self.output, "activity start {activity:?} {generation}").unwrap(),
            StateTreeTraceRecord::ActivityCancelled {
                activity,
                generation,
            } => writeln!(self.output, "activity cancel {activity:?} {generation}").unwrap(),
            StateTreeTraceRecord::ActivityOutcome {
                activity,
                success,
                generation,
            } => writeln!(
                self.output,
                "activity outcome {activity:?} {success} {generation}"
            )
            .unwrap(),
            StateTreeTraceRecord::ConfigurationStarted => {
                self.output.push_str("configuration\n");
            }
            StateTreeTraceRecord::ActiveState { state } => {
                writeln!(self.output, "active {state:?}").unwrap();
            }
            StateTreeTraceRecord::Status { status } => match status {
                InstanceStatus::Pending => self.output.push_str("status pending\n"),
                InstanceStatus::Running => self.output.push_str("status running\n"),
                InstanceStatus::Complete => self.output.push_str("status complete\n"),
                InstanceStatus::Faulted(fault) => {
                    self.output.push_str("status faulted ");
                    write_fault(&mut self.output, fault);
                    self.output.push('\n');
                }
            },
            StateTreeTraceRecord::MacrostepEnded => self.output.push_str("macrostep end\n"),
        }
    }
}

fn write_fault(output: &mut String, fault: &RuntimeFault) {
    match fault {
        RuntimeFault::MicrostepLimitExceeded { limit } => {
            write!(output, "microstep-limit {limit}").unwrap();
        }
        RuntimeFault::Datamodel {
            operation,
            span_start: _,
            message: _,
        } => {
            write!(output, "datamodel {operation:?}").unwrap();
        }
        RuntimeFault::Effect { effect, .. } => {
            write!(output, "effect {effect:?}").unwrap();
        }
        RuntimeFault::EventDataProjection { event, .. } => {
            write!(output, "event-data {event:?}").unwrap();
        }
        RuntimeFault::Activity { activity, .. } => write!(output, "activity {activity:?}").unwrap(),
        RuntimeFault::ActivityDataProjection { activity, .. } => {
            write!(output, "activity-data {activity:?}").unwrap()
        }
    }
}

fn write_luau_value(output: &mut String, value: &LuauValue) {
    match value {
        LuauValue::Nil => output.push_str("nil"),
        LuauValue::Boolean(value) => write!(output, "{value}").unwrap(),
        LuauValue::Integer(value) => write!(output, "{value}").unwrap(),
        LuauValue::Number(value) => write!(output, "{value:?}").unwrap(),
        LuauValue::String(value) => write!(output, "{value:?}").unwrap(),
        LuauValue::Sequence(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    output.push_str(", ");
                }
                write_luau_value(output, value);
            }
            output.push(']');
        }
        LuauValue::Record(values) => {
            output.push('{');
            for (index, (name, value)) in values.iter().enumerate() {
                if index != 0 {
                    output.push_str(", ");
                }
                write!(output, "{name:?}: ").unwrap();
                write_luau_value(output, value);
            }
            output.push('}');
        }
    }
}
