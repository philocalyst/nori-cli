//! Declarative goal policy: who checks that a goal is really done, and how far the agent and the
//! checker may disagree before the person decides.
//!
//! An agent marks its goal complete with `update_goal`; with `judge` on, the harness first has a
//! fresh reviewer read the transcript and decide (see `nori-harness`'s `goal_judge`). Layers
//! (built-in default, user, project) are [`GoalsOverride`]s laid over one [`GoalsPolicy`] field by
//! field, as `compaction` is.

use std::time::Duration;

/// The longest a judge may be given. Agents' own tool calls are bounded far below this; a
/// reviewer that has not answered in ten minutes is not going to.
pub const MAX_JUDGE_TIMEOUT: Duration = Duration::from_secs(600);

/// Goal handling in force for one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalsPolicy {
    /// An independent reviewer decides whether a goal the agent calls complete really is.
    /// `false` accepts the agent's word.
    pub judge: bool,
    /// The agent the reviewer runs on, as in Nori's agent configuration (`claude-code`,
    /// `codex`, ...). `None`: the session's own agent.
    pub judge_agent: Option<String>,
    /// The reviewer's model. `None`: that agent's default model.
    pub judge_model: Option<String>,
    /// A reviewer that has not answered in this long is given up on, and the agent's word is
    /// accepted (marked unverified).
    pub judge_timeout: Duration,
    /// After this many verdicts in a row that a goal is not done yet, the next `complete`
    /// pauses the goal and the person decides.
    pub max_disagreements: i64,
}

impl Default for GoalsPolicy {
    /// The built-in policy; `pkl/default.pkl` states the same values and a test keeps them equal.
    fn default() -> Self {
        Self {
            judge: true,
            judge_agent: None,
            judge_model: None,
            judge_timeout: Duration::from_secs(120),
            max_disagreements: 3,
        }
    }
}

impl GoalsPolicy {
    /// Why this policy cannot be applied, if it cannot.
    pub fn validate(&self) -> Result<(), String> {
        if self.judge_agent.as_deref().is_some_and(blank) {
            return Err("judge_agent must name an agent, or be null for the session's own".into());
        }
        if self.judge_model.as_deref().is_some_and(blank) {
            return Err("judge_model must name a model, or be null for the agent's default".into());
        }
        if self.judge_timeout.is_zero() || self.judge_timeout > MAX_JUDGE_TIMEOUT {
            return Err(format!(
                "judge_timeout must be above zero and at most {} seconds, got {} seconds",
                MAX_JUDGE_TIMEOUT.as_secs(),
                self.judge_timeout.as_secs_f64()
            ));
        }
        if self.max_disagreements < 1 {
            return Err("max_disagreements must be at least 1".to_owned());
        }
        Ok(())
    }
}

fn blank(text: &str) -> bool {
    text.trim().is_empty()
}

/// Fields a policy layer sets; unset fields keep the layer below. `judge_agent` and `judge_model`
/// are nullable in policy, so a layer can also set them back to null: the outer `Option` is
/// whether the layer says anything, the inner one is its value.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GoalsOverride {
    pub judge: Option<bool>,
    pub judge_agent: Option<Option<String>>,
    pub judge_model: Option<Option<String>>,
    pub judge_timeout: Option<Duration>,
    pub max_disagreements: Option<i64>,
}

impl GoalsOverride {
    /// Lay `self` over `base`: every field it sets replaces the base's.
    pub fn apply(self, base: &mut GoalsPolicy) {
        if let Some(judge) = self.judge {
            base.judge = judge;
        }
        if let Some(agent) = self.judge_agent {
            base.judge_agent = agent;
        }
        if let Some(model) = self.judge_model {
            base.judge_model = model;
        }
        if let Some(timeout) = self.judge_timeout {
            base.judge_timeout = timeout;
        }
        if let Some(max) = self.max_disagreements {
            base.max_disagreements = max;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn an_override_replaces_only_what_it_sets_and_can_set_null() {
        let mut policy = GoalsPolicy {
            judge_agent: Some("codex".into()),
            judge_model: Some("mini".into()),
            ..GoalsPolicy::default()
        };
        GoalsOverride {
            judge_agent: Some(None),
            max_disagreements: Some(5),
            ..GoalsOverride::default()
        }
        .apply(&mut policy);
        assert_eq!(
            policy,
            GoalsPolicy {
                judge_agent: None,
                judge_model: Some("mini".into()),
                max_disagreements: 5,
                ..GoalsPolicy::default()
            }
        );
    }

    #[test]
    fn a_policy_that_could_not_be_applied_is_refused() {
        assert_eq!(GoalsPolicy::default().validate(), Ok(()));
        for policy in [
            GoalsPolicy {
                judge_agent: Some("  ".into()),
                ..GoalsPolicy::default()
            },
            GoalsPolicy {
                judge_model: Some(String::new()),
                ..GoalsPolicy::default()
            },
            GoalsPolicy {
                judge_timeout: Duration::ZERO,
                ..GoalsPolicy::default()
            },
            GoalsPolicy {
                judge_timeout: MAX_JUDGE_TIMEOUT + Duration::from_secs(1),
                ..GoalsPolicy::default()
            },
            GoalsPolicy {
                max_disagreements: 0,
                ..GoalsPolicy::default()
            },
        ] {
            assert!(policy.validate().is_err(), "{policy:?}");
        }
    }
}
