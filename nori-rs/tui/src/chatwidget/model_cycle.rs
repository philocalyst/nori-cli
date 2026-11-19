//! Next and previous model (Ctrl+N / Ctrl+P by default) and the next effort level (Shift+Space),
//! without opening a picker.
//!
//! A press shows the new value at once: the agent's advertised configuration is updated in
//! place, so the footer changes on the keypress and the agent's own echo of the change finds
//! nothing new to report. The agent is told in the background, one request at a time: presses
//! that come while a request is out only move the target, which is sent when the request
//! returns, so a quick run of presses costs two requests at most. If the agent refuses, the
//! configuration from before the run comes back.

use nori_protocol::acp::v1 as acp;

use super::ChatWidget;
use crate::nori::session_config_picker::ConfigPickerOrigin;

/// Which way a press steps through the agent's models.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelStep {
    Next,
    Previous,
}

/// What a press changes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum CycledOption {
    #[default]
    Model,
    /// The reasoning effort or thought level.
    Effort,
}

impl CycledOption {
    /// Whether `option` is the agent's selector for this. Effort is the option the agent
    /// declares as its thought level; an agent that declares no category for it is found by
    /// the name it gives it.
    fn selects(self, option: &acp::SessionConfigOption) -> bool {
        match self {
            Self::Model => option.category == Some(acp::SessionConfigOptionCategory::Model),
            Self::Effort => crate::nori::agent_config_state::is_effort_option(option),
        }
    }

    fn noun(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Effort => "effort level",
        }
    }
}

/// A run of model presses still being applied.
#[derive(Debug, Default)]
pub(crate) struct ModelCycle {
    /// A request is out; its result settles it.
    in_flight: bool,
    /// Where the presses since the request went out point, one target per option.
    queued: Vec<Target>,
    /// What the run changes, for the words shown if the agent refuses.
    /// The configuration before the run's first press, restored if the agent refuses.
    before: Option<Vec<acp::SessionConfigOption>>,
}

/// Presses made before the agent listed its models. The placeholder widget they land on is
/// replaced when the session starts, so the app keeps them and hands them to the widget that
/// has the list.
#[derive(Debug, Default)]
pub(crate) struct WaitingModelPresses {
    /// Next presses minus previous ones.
    net: i32,
    last: Option<std::time::Instant>,
}

/// How long a press waits for the list before it is forgotten, so that a press made on an agent
/// that never lists models does not surface on the next agent.
const PRESS_STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(10);

impl WaitingModelPresses {
    /// Keep a press; true when it starts a run (the user is told once per run).
    pub(crate) fn press(&mut self, step: ModelStep, now: std::time::Instant) -> bool {
        let starts_run = self.last.is_none() || self.is_stale(now);
        if starts_run {
            self.net = 0;
        }
        self.net += match step {
            ModelStep::Next => 1,
            ModelStep::Previous => -1,
        };
        self.last = Some(now);
        starts_run
    }

    pub(crate) fn is_waiting(&self) -> bool {
        self.last.is_some()
    }

    /// The presses to make now: the direction and how many; none when they went stale or
    /// cancelled out.
    pub(crate) fn take(&mut self, now: std::time::Instant) -> Option<(ModelStep, u32)> {
        let stale = self.is_stale(now);
        let net = std::mem::take(&mut self.net);
        self.last = None;
        if stale || net == 0 {
            return None;
        }
        let step = if net > 0 {
            ModelStep::Next
        } else {
            ModelStep::Previous
        };
        Some((step, net.unsigned_abs()))
    }

    fn is_stale(&self, now: std::time::Instant) -> bool {
        self.last
            .is_some_and(|last| now.saturating_duration_since(last) > PRESS_STALE_AFTER)
    }
}

#[derive(Debug, Clone)]
struct Target {
    config_id: String,
    value: String,
    option_name: String,
    value_name: String,
}

impl ChatWidget {
    /// Step to the next or previous model. False, with nothing done, while the agent has not
    /// listed its options yet.
    pub(crate) fn cycle_model(&mut self, step: ModelStep) -> bool {
        self.cycle_option(CycledOption::Model, step)
    }

    /// Step to the next or previous effort level, as [`Self::cycle_model`] does for models.
    pub(crate) fn cycle_effort(&mut self, step: ModelStep) -> bool {
        self.cycle_option(CycledOption::Effort, step)
    }

    fn cycle_option(&mut self, kind: CycledOption, step: ModelStep) -> bool {
        if self.agent_config().is_empty() {
            return false;
        }
        let noun = kind.noun();
        let mut options = self.agent_config().raw_options();
        let Some(option) = options.iter_mut().find(|option| kind.selects(option)) else {
            self.app_event_tx
                .send(crate::app_event::AppEvent::TransientNotice(format!(
                    "This agent doesn't offer a choice of {noun}."
                )));
            return true;
        };
        let option_name = option.name.clone();
        let config_id = option.id.to_string();
        let acp::SessionConfigKind::Select(select) = &mut option.kind else {
            return true;
        };
        let offered: Vec<(String, String)> = match &select.options {
            acp::SessionConfigSelectOptions::Ungrouped(values) => values
                .iter()
                .map(|value| (value.value.to_string(), value.name.clone()))
                .collect(),
            acp::SessionConfigSelectOptions::Grouped(groups) => groups
                .iter()
                .flat_map(|group| group.options.iter())
                .map(|value| (value.value.to_string(), value.name.clone()))
                .collect(),
            // The schema's option layouts are not exhaustive; an unknown one offers nothing
            // that can be stepped through.
            _ => Vec::new(),
        };
        if offered.len() < 2 {
            self.add_info_message(format!("This agent offers one {noun}."), None);
            return true;
        }
        let current = select.current_value.to_string();
        let position = offered
            .iter()
            .position(|(value, _)| *value == current)
            .unwrap_or(0);
        let next = match step {
            ModelStep::Next => (position + 1) % offered.len(),
            ModelStep::Previous => (position + offered.len() - 1) % offered.len(),
        };
        let (value, value_name) = offered[next].clone();
        select.current_value = value.clone().into();

        if self.model_cycle.before.is_none() {
            self.model_cycle.before = Some(self.agent_config().raw_options());
        }
        self.sync_acp_session_config_snapshot(&options);
        let target = Target {
            config_id,
            value,
            option_name,
            value_name,
        };
        if self.model_cycle.in_flight {
            // The newest target for an option replaces an older one; another option's stays.
            self.model_cycle
                .queued
                .retain(|queued| queued.config_id != target.config_id);
            self.model_cycle.queued.push(target);
        } else {
            self.send_model_target(target);
        }
        self.request_redraw();
        true
    }

    fn send_model_target(&mut self, target: Target) {
        self.model_cycle.in_flight = true;
        self.set_acp_session_config_option(
            target.config_id,
            target.value,
            target.option_name,
            target.value_name,
            false,
            ConfigPickerOrigin::Cycle,
            None,
        );
    }

    /// The agent answered a model switch made by a press, with the configuration it now has.
    /// While more presses are queued their target stands, so the answer is not shown.
    pub(crate) fn on_model_cycle_result(
        &mut self,
        result: Result<Option<Vec<acp::SessionConfigOption>>, String>,
    ) {
        if !self.model_cycle.queued.is_empty() {
            let target = self.model_cycle.queued.remove(0);
            self.send_model_target(target);
            return;
        }
        self.model_cycle.in_flight = false;
        let before = self.model_cycle.before.take();
        match result {
            Ok(Some(options)) => self.sync_acp_session_config_snapshot(&options),
            Ok(None) => {}
            Err(_error) => {
                if let Some(before) = before {
                    self.sync_acp_session_config_snapshot(&before);
                }
            }
        }
        self.request_redraw();
    }
}
