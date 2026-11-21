//! Next and previous model (Ctrl+N / Ctrl+P by default), effort level (Ctrl+J / Ctrl+K) and model
//! mode (Alt+. / Alt+,), without opening a picker.
//!
//! A press shows the new value at once: the agent's advertised configuration is updated in
//! place, so the footer changes on the keypress and the agent's own echo of the change finds
//! nothing new to report. The agent is told in the background, one request at a time: presses
//! that come while a request is out only move the target, which is sent when the request
//! returns, so a quick run of presses costs two requests at most. If the agent refuses, the
//! configuration from before the run comes back. Nothing here waits for a turn to end.
//!
//! A value chosen in `/model` or `/config` is shown the same way, for the same reason: the footer
//! changes when the row is chosen ([`ChatWidget::show_chosen_option`]), not when the agent
//! answers, and the answer ([`ChatWidget::settle_chosen_option`]) either confirms it or puts the
//! old value back.
//!
//! What a press steps through, and in which order, is decided by `nori_protocol::ladder`: models in
//! the order `/model` lists them, effort from least to most, modes from slowest to fastest. Past
//! either end the step wraps around, and says so.

use nori_protocol::Dial;
use nori_protocol::Direction;
use nori_protocol::acp::v1 as acp;

use super::ChatWidget;
use crate::nori::session_config_picker::ConfigPickerOrigin;

/// Which way a press steps through the agent's models.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelStep {
    Next,
    Previous,
}

/// A run of model presses still being applied.
#[derive(Debug, Default)]
pub(crate) struct ModelCycle {
    /// A request is out; its result settles it.
    in_flight: bool,
    /// Where the presses since the request went out point, one target per option.
    queued: Vec<Target>,
    /// The configuration before the run's first press, restored if the agent refuses.
    before: Option<Vec<acp::SessionConfigOption>>,
    /// Values chosen in a picker that the footer already shows and the agent has not answered
    /// for, as `(option id, value)`.
    chosen: Vec<(String, String)>,
}

impl ModelCycle {
    /// Whether a run of presses has not been settled by the agent yet. What the agent announces
    /// in the meantime is older than the presses on screen, so it is not adopted.
    pub(crate) fn is_settling(&self) -> bool {
        self.in_flight || !self.queued.is_empty() || !self.chosen.is_empty()
    }
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
}

impl ChatWidget {
    /// Step to the next or previous model. False, with nothing done, while the agent has not
    /// listed its options yet.
    pub(crate) fn cycle_model(&mut self, step: ModelStep) -> bool {
        self.cycle_option(Dial::Model, step)
    }

    /// Step to the next or previous effort level, as [`Self::cycle_model`] does for models.
    pub(crate) fn cycle_effort(&mut self, step: ModelStep) -> bool {
        self.cycle_option(Dial::Effort, step)
    }

    /// Step to the next or previous model mode (flex, standard, fast, priority).
    pub(crate) fn cycle_model_mode(&mut self, step: ModelStep) -> bool {
        self.cycle_option(Dial::Mode, step)
    }

    fn cycle_option(&mut self, dial: Dial, step: ModelStep) -> bool {
        let direction = match step {
            ModelStep::Next => Direction::Forward,
            ModelStep::Previous => Direction::Backward,
        };
        let only_one = format!("This agent offers one {}.", dial.noun());
        self.choose_option(dial, only_one, |rungs, current| {
            nori_protocol::step(rungs, current, direction)
                .map(|stepped| (stepped.to, stepped.wrapped))
        })
    }

    /// Set `dial` to the value `wanted` names (its value or label, ignoring case; else the one
    /// value whose label contains it), as a press of the dial's key would. False, with nothing
    /// done, while the agent has not listed its options yet.
    pub(crate) fn set_dial(&mut self, dial: Dial, wanted: &str) -> bool {
        let wanted = wanted.trim().to_lowercase();
        let missing = format!("No {} matches \u{201c}{wanted}\u{201d}.", dial.noun());
        self.choose_option(dial, missing, |rungs, _| {
            let exact = rungs.iter().find(|rung| {
                rung.value.to_lowercase() == wanted || rung.label.to_lowercase() == wanted
            });
            let partial = || {
                let mut found = rungs.iter().filter(|rung| {
                    rung.label.to_lowercase().contains(&wanted)
                        || rung.value.to_lowercase().contains(&wanted)
                });
                match (found.next(), found.next()) {
                    (Some(rung), None) => Some(rung),
                    _ => None,
                }
            };
            exact.or_else(partial).map(|rung| (rung.clone(), false))
        })
    }

    /// What setting `dial` to `value` does now: when it takes hold and what it costs. A dial
    /// change is never refused, so this never fails; the answer is shown where the change is
    /// made.
    ///
    /// It is read from the agent's options as last advertised: the effect the choice declares
    /// (`_meta.nori.dial`), else [`nori_protocol::DialEffect::next_turn`]. A model the agent does
    /// not list is another agent's, which the session moves to: when the running turn ends
    /// (`turn_running`), else at once, and either way the new agent starts a session of its own.
    pub(crate) fn dial_effect(
        &self,
        dial: Dial,
        value: &str,
        turn_running: bool,
    ) -> nori_protocol::DialEffect {
        let options = self.agent_config().raw_options();
        if let Some(effect) = nori_protocol::DialEffect::declared_for(&options, dial, value) {
            return effect;
        }
        let lists_models = options.iter().any(|option| Dial::Model.selects(option));
        if dial == Dial::Model && lists_models {
            return nori_protocol::DialEffect {
                when: if turn_running {
                    nori_protocol::AppliesAt::AfterThisTurn
                } else {
                    nori_protocol::AppliesAt::Now
                },
                costs: vec![nori_protocol::Consequence::SessionRestarts],
            };
        }
        nori_protocol::DialEffect::next_turn()
    }

    /// Move `dial` to the value `pick` chooses from its ladder and the current value; `missing`
    /// is said when it chooses none.
    fn choose_option(
        &mut self,
        dial: Dial,
        missing: String,
        pick: impl FnOnce(&[nori_protocol::Rung], &str) -> Option<(nori_protocol::Rung, bool)>,
    ) -> bool {
        if self.agent_config().is_empty() {
            return false;
        }
        let noun = dial.noun();
        let mut options = self.agent_config().raw_options();
        let Some(option) = options.iter_mut().find(|option| dial.selects(option)) else {
            self.app_event_tx
                .send(crate::app_event::AppEvent::TransientNotice(format!(
                    "This agent doesn't offer a choice of {noun}."
                )));
            return true;
        };
        let option_name = option.name.clone();
        let config_id = option.id.to_string();
        let rungs = nori_protocol::ladder(dial, option);
        let acp::SessionConfigKind::Select(select) = &mut option.kind else {
            return true;
        };
        let Some((to, wrapped)) = pick(&rungs, &select.current_value.to_string()) else {
            self.add_info_message(missing, None);
            return true;
        };
        let value_name = to.label.clone();
        let value = to.value;
        select.current_value = value.clone().into();
        // Shown on this keypress; the agent is told in the background.
        crate::latency::begin(crate::latency::Interaction::ConfigSwitch, true);

        if self.model_cycle.before.is_none() {
            self.model_cycle.before = Some(self.agent_config().raw_options());
        }
        self.sync_acp_session_config_snapshot(&options);
        if wrapped {
            self.app_event_tx
                .send(crate::app_event::AppEvent::TransientNotice(format!(
                    "Wrapped around to {value_name}"
                )));
        }
        let target = Target {
            config_id,
            value,
            option_name,
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

    /// A value chosen in a picker is sent to the agent: show it now, as a press does. Nothing
    /// happens for a value the agent has not listed (a custom model it may refuse) or that is
    /// already current.
    pub(crate) fn show_chosen_option(&mut self, config_id: &str, value: &str) {
        let mut options = self.agent_config().raw_options();
        let Some(option) = options
            .iter_mut()
            .find(|option| option.id.to_string() == config_id)
        else {
            return;
        };
        let acp::SessionConfigKind::Select(select) = &mut option.kind else {
            return;
        };
        let offered = match &select.options {
            acp::SessionConfigSelectOptions::Ungrouped(values) => values
                .iter()
                .any(|offered| offered.value.to_string() == value),
            acp::SessionConfigSelectOptions::Grouped(groups) => groups.iter().any(|group| {
                group
                    .options
                    .iter()
                    .any(|offered| offered.value.to_string() == value)
            }),
            _ => false,
        };
        if !offered || select.current_value.to_string() == value {
            return;
        }
        select.current_value = value.to_owned().into();
        crate::latency::begin(crate::latency::Interaction::ConfigSwitch, true);
        if self.model_cycle.before.is_none() {
            self.model_cycle.before = Some(self.agent_config().raw_options());
        }
        self.model_cycle
            .chosen
            .push((config_id.to_owned(), value.to_owned()));
        self.sync_acp_session_config_snapshot(&options);
        self.request_redraw();
    }

    /// The agent answered a value chosen in a picker. `true` when the configuration it returned
    /// should be adopted now; `false` while later choices are still out, whose values stand, or
    /// when the choice was refused, in which case the configuration from before comes back.
    pub(crate) fn settle_chosen_option(
        &mut self,
        config_id: &str,
        value: &str,
        succeeded: bool,
    ) -> bool {
        if let Some(position) = self
            .model_cycle
            .chosen
            .iter()
            .position(|(id, chosen)| id == config_id && chosen == value)
        {
            self.model_cycle.chosen.remove(position);
        }
        if self.model_cycle.is_settling() {
            return false;
        }
        let before = self.model_cycle.before.take();
        if succeeded {
            return true;
        }
        if let Some(before) = before {
            self.sync_acp_session_config_snapshot(&before);
            self.request_redraw();
        }
        false
    }
}
