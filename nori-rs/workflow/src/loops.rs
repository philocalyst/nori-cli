//! Declarative loop-detection policy: what a session does when its agent keeps making the same
//! tool call without getting anywhere.
//!
//! The harness keeps a signature per tool call of a turn (see `nori-harness`'s turn ledger) and
//! asks this policy what to do when one signature fills more than `repeats` of the last `window`
//! calls. Layers are [`LoopOverride`]s laid over one [`LoopPolicy`] field by field.

/// What happens when a loop is found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LoopAction {
    /// Steer the agent once; if it loops again in the same task, cancel the turn.
    NudgeThenStop,
    /// Steer the agent once per task and otherwise let it go on.
    Nudge,
    /// Cancel the turn at once, as the user's Esc does.
    Stop,
    /// Do not look for loops.
    Off,
}

impl LoopAction {
    pub const ALL: [LoopAction; 4] = [
        LoopAction::NudgeThenStop,
        LoopAction::Nudge,
        LoopAction::Stop,
        LoopAction::Off,
    ];

    /// The name policy files use, such as `nudge_then_stop`.
    pub fn name(self) -> &'static str {
        match self {
            LoopAction::NudgeThenStop => "nudge_then_stop",
            LoopAction::Nudge => "nudge",
            LoopAction::Stop => "stop",
            LoopAction::Off => "off",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|action| action.name() == name)
    }
}

/// Loop detection in force for one session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoopPolicy {
    /// How many of a turn's latest tool calls are looked at.
    pub window: i64,
    /// A call counts as a loop when its signature occurs more than this many times in the window.
    pub repeats: i64,
    pub action: LoopAction,
}

impl Default for LoopPolicy {
    /// The built-in policy; `pkl/default.pkl` states the same values and a test keeps them equal.
    fn default() -> Self {
        Self {
            window: 10,
            repeats: 5,
            action: LoopAction::NudgeThenStop,
        }
    }
}

impl LoopPolicy {
    /// Why this policy cannot be applied, if it cannot.
    pub fn validate(&self) -> Result<(), String> {
        if self.window < 1 {
            return Err("window must be at least 1".to_owned());
        }
        if self.repeats < 1 {
            return Err("repeats must be at least 1".to_owned());
        }
        if self.repeats >= self.window {
            return Err(format!(
                "repeats ({}) must be less than window ({}), or no loop could ever be found",
                self.repeats, self.window
            ));
        }
        Ok(())
    }
}

/// Fields a policy layer sets; unset fields keep the layer below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LoopOverride {
    pub window: Option<i64>,
    pub repeats: Option<i64>,
    pub action: Option<LoopAction>,
}

impl LoopOverride {
    /// Lay `self` over `base`: every field it sets replaces the base's.
    pub fn apply(self, base: &mut LoopPolicy) {
        if let Some(window) = self.window {
            base.window = window;
        }
        if let Some(repeats) = self.repeats {
            base.repeats = repeats;
        }
        if let Some(action) = self.action {
            base.action = action;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn actions_round_trip_through_their_policy_names() {
        for action in LoopAction::ALL {
            assert_eq!(LoopAction::parse(action.name()), Some(action));
        }
        assert_eq!(LoopAction::parse("panic"), None);
    }

    #[test]
    fn an_override_replaces_only_what_it_sets() {
        let mut policy = LoopPolicy::default();
        LoopOverride {
            repeats: Some(3),
            ..LoopOverride::default()
        }
        .apply(&mut policy);
        assert_eq!(
            policy,
            LoopPolicy {
                repeats: 3,
                ..LoopPolicy::default()
            }
        );
    }

    #[test]
    fn a_policy_that_could_never_fire_is_refused() {
        assert_eq!(LoopPolicy::default().validate(), Ok(()));
        for policy in [
            LoopPolicy {
                window: 0,
                ..LoopPolicy::default()
            },
            LoopPolicy {
                repeats: 0,
                ..LoopPolicy::default()
            },
            LoopPolicy {
                window: 5,
                repeats: 5,
                ..LoopPolicy::default()
            },
        ] {
            assert!(policy.validate().is_err(), "{policy:?}");
        }
    }
}
