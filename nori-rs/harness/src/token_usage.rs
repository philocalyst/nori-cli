//! The tokens a session used, as the agent reports them.

/// Token usage breakdown of a session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TranscriptTokenUsage {
    /// Total input tokens.
    pub input_tokens: i64,
    /// Total output tokens.
    pub output_tokens: i64,
    /// Cached input tokens (subset of input_tokens).
    pub cached_tokens: i64,
    /// Context window fill: the input-side tokens of the most recent turn. `None` when unknown.
    pub last_context_tokens: Option<i64>,
}

impl TranscriptTokenUsage {
    /// Returns the total tokens (input + output).
    pub fn total(&self) -> i64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}
