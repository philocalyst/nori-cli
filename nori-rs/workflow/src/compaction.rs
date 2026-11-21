//! Declarative compaction policy: when a session compacts itself, and how a refused checkpoint
//! is cut down.
//!
//! Policy is plain data (written in Pkl, see `docs/workflows.md`); the harness applies it at turn
//! boundaries. Layers (built-in default, user, project) are [`CompactionOverride`]s laid over one
//! [`CompactionPolicy`] field by field, as `recovery` is.

/// Compaction behaviour in force for one session.
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionPolicy {
    /// Compact by itself when the context nears the window's end.
    pub auto: bool,
    /// Compact when the free context is below this fraction of the window...
    pub trigger_free_fraction: f64,
    /// ...or below this many tokens, whichever asks for more room.
    pub trigger_free_tokens: i64,
    /// Start preparing the summary this many tokens of free context before the trigger, while the
    /// provider's prompt cache is still warm, so the switch is instant. `0` prepares nothing early.
    pub background_margin_tokens: i64,
    /// On a prompt-too-long refusal, retry with the checkpoint cut to each of these fractions of
    /// its size in turn. Empty: no retry with a shorter checkpoint.
    pub shrink_ladder: Vec<f64>,
    /// Use the agent's own `/compact` when it advertises one.
    pub prefer_native: bool,
}

// Every float is validated finite wherever policy is loaded, so equality is total.
impl Eq for CompactionPolicy {}

impl Default for CompactionPolicy {
    /// The built-in policy; `pkl/default.pkl` states the same values and a test keeps them equal.
    fn default() -> Self {
        Self {
            auto: true,
            trigger_free_fraction: 0.15,
            trigger_free_tokens: 16_000,
            background_margin_tokens: 12_000,
            shrink_ladder: vec![0.85, 0.7, 0.5, 0.35],
            prefer_native: true,
        }
    }
}

/// How full a session's context is, against the policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextPressure {
    /// Plenty of room.
    Comfortable,
    /// Within the background margin of the trigger: the summary can be prepared now.
    Approaching,
    /// Below the trigger: compact before the next prompt.
    Due,
}

impl CompactionPolicy {
    /// The free context, in tokens, below which a window of `window_tokens` compacts:
    /// the larger of the fraction of the window and the token floor (at most a quarter of it).
    pub fn trigger_free(&self, window_tokens: i64) -> i64 {
        let fraction = (self.trigger_free_fraction * window_tokens as f64).ceil() as i64;
        // A window too small for the token floor would always be due: the floor asks for at most
        // a quarter of it.
        fraction.max(self.trigger_free_tokens.min(window_tokens / 4))
    }

    /// Where `used_tokens` of a `window_tokens` window stands. An unknown window (not above zero)
    /// is never under pressure: there is nothing to measure the free context against.
    pub fn pressure(&self, used_tokens: i64, window_tokens: i64) -> ContextPressure {
        if window_tokens <= 0 {
            return ContextPressure::Comfortable;
        }
        let free = window_tokens.saturating_sub(used_tokens);
        let trigger = self.trigger_free(window_tokens);
        if free < trigger {
            ContextPressure::Due
        } else if self.background_margin_tokens > 0
            && free < trigger.saturating_add(self.background_margin_tokens.min(window_tokens / 4))
        {
            ContextPressure::Approaching
        } else {
            ContextPressure::Comfortable
        }
    }

    /// Why this policy cannot be applied, if it cannot.
    pub fn validate(&self) -> Result<(), String> {
        if !(0.0..=1.0).contains(&self.trigger_free_fraction) {
            return Err(format!(
                "trigger_free_fraction must be between 0 and 1, got {}",
                self.trigger_free_fraction
            ));
        }
        if self.trigger_free_tokens < 0 {
            return Err("trigger_free_tokens must be nonnegative".to_owned());
        }
        if self.background_margin_tokens < 0 {
            return Err("background_margin_tokens must be nonnegative".to_owned());
        }
        let mut previous = 1.0;
        for fraction in &self.shrink_ladder {
            if !(*fraction > 0.0 && *fraction < previous) {
                return Err(format!(
                    "shrink_ladder must fall strictly from just under 1 toward 0, got {fraction} after {previous}"
                ));
            }
            previous = *fraction;
        }
        Ok(())
    }
}

/// Fields a policy layer sets; unset fields keep the layer below.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CompactionOverride {
    pub auto: Option<bool>,
    pub trigger_free_fraction: Option<f64>,
    pub trigger_free_tokens: Option<i64>,
    pub background_margin_tokens: Option<i64>,
    /// Replaced as a whole value, like every list in policy.
    pub shrink_ladder: Option<Vec<f64>>,
    pub prefer_native: Option<bool>,
}

impl CompactionOverride {
    /// Lay `self` over `base`: every field it sets replaces the base's.
    pub fn apply(self, base: &mut CompactionPolicy) {
        if let Some(auto) = self.auto {
            base.auto = auto;
        }
        if let Some(fraction) = self.trigger_free_fraction {
            base.trigger_free_fraction = fraction;
        }
        if let Some(tokens) = self.trigger_free_tokens {
            base.trigger_free_tokens = tokens;
        }
        if let Some(margin) = self.background_margin_tokens {
            base.background_margin_tokens = margin;
        }
        if let Some(ladder) = self.shrink_ladder {
            base.shrink_ladder = ladder;
        }
        if let Some(prefer_native) = self.prefer_native {
            base.prefer_native = prefer_native;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn the_trigger_is_the_larger_of_the_fraction_and_the_token_floor() {
        let policy = CompactionPolicy::default();
        assert_eq!(policy.trigger_free(200_000), 30_000);
        assert_eq!(policy.trigger_free(60_000), 16_000);
        assert_eq!(policy.trigger_free(1_000_000), 150_000);
    }

    #[test]
    fn pressure_has_three_bands_and_ignores_an_unknown_window() {
        let policy = CompactionPolicy::default();
        // 200k window: due under 30k free, approaching under 42k free (the 12k margin).
        assert_eq!(
            policy.pressure(158_000, 200_000),
            ContextPressure::Comfortable
        );
        assert_eq!(
            policy.pressure(158_001, 200_000),
            ContextPressure::Approaching
        );
        assert_eq!(policy.pressure(170_001, 200_000), ContextPressure::Due);
        assert_eq!(
            policy.pressure(170_000, 200_000),
            ContextPressure::Approaching
        );
        assert_eq!(policy.pressure(900_000, 0), ContextPressure::Comfortable);
        let no_margin = CompactionPolicy {
            background_margin_tokens: 0,
            ..CompactionPolicy::default()
        };
        assert_eq!(
            no_margin.pressure(120_000, 200_000),
            ContextPressure::Comfortable
        );
    }

    #[test]
    fn an_override_replaces_only_what_it_sets() {
        let mut policy = CompactionPolicy::default();
        CompactionOverride {
            auto: Some(false),
            shrink_ladder: Some(vec![0.5]),
            ..CompactionOverride::default()
        }
        .apply(&mut policy);
        assert_eq!(
            policy,
            CompactionPolicy {
                auto: false,
                shrink_ladder: vec![0.5],
                ..CompactionPolicy::default()
            }
        );
    }

    #[test]
    fn validation_names_the_field_that_is_wrong() {
        assert_eq!(CompactionPolicy::default().validate(), Ok(()));
        let bad = |policy: CompactionPolicy| policy.validate().unwrap_err();
        assert!(
            bad(CompactionPolicy {
                trigger_free_fraction: 1.5,
                ..CompactionPolicy::default()
            })
            .contains("trigger_free_fraction")
        );
        assert!(
            bad(CompactionPolicy {
                trigger_free_tokens: -1,
                ..CompactionPolicy::default()
            })
            .contains("trigger_free_tokens")
        );
        assert!(
            bad(CompactionPolicy {
                shrink_ladder: vec![0.5, 0.7],
                ..CompactionPolicy::default()
            })
            .contains("shrink_ladder")
        );
        assert!(
            bad(CompactionPolicy {
                shrink_ladder: vec![1.0],
                ..CompactionPolicy::default()
            })
            .contains("shrink_ladder")
        );
        assert_eq!(
            CompactionPolicy {
                shrink_ladder: Vec::new(),
                ..CompactionPolicy::default()
            }
            .validate(),
            Ok(())
        );
    }
}
