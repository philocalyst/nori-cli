//! The one budget for conversation history that Nori puts into an agent's prompt.
//!
//! A fork, a restored child, a rewind, a route fallback, a resume that replays the transcript
//! and a compaction all hand an agent earlier conversation as text. Providers refuse a prompt
//! whose text blocks are too large (OpenAI's Responses API caps one string at 10 MiB, long
//! before a model's window is full), and an agent that is given most of its window as history has
//! little left to work in. So injected history is bounded by design, here, and not by each
//! caller.
//!
//! The rule: history may take a quarter of the target model's window, at four bytes a token,
//! which is the window's token count in bytes. It never takes less than [`ContextBudget::FLOOR`]
//! or more than [`ContextBudget::CEILING`], and a model whose window is unknown is treated as
//! having [`ContextBudget::UNKNOWN_WINDOW_TOKENS`]. The ceiling is a tenth of the provider's
//! string limit. What does not fit is left out, newest history first kept, and each cut says
//! how many bytes it left out.
//!
//! Three things build on the budget:
//! - `transcript::context_digest` renders an event log within it (forks, restores, rewinds).
//! - [`fit_summary`] bounds any summary text, as the last guard on every checkpoint.
//! - [`shrink_checkpoint`] cuts an already built checkpoint to a fraction of its size, for a
//!   retry after a provider says the prompt was too long anyway.

use std::borrow::Cow;

use nori_protocol::acp::v1 as acp;

/// How much conversation history may go into one prompt, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextBudget {
    bytes: usize,
}

impl ContextBudget {
    /// The most history ever injected: a tenth of a provider's 10 MiB string limit.
    pub const CEILING: usize = 1024 * 1024;
    /// The least history worth injecting, however small the window.
    pub const FLOOR: usize = 32 * 1024;
    /// The window assumed for a model that does not say.
    pub const UNKNOWN_WINDOW_TOKENS: i64 = 200_000;

    /// The budget for a model whose context window is `window_tokens` (`None` when unknown):
    /// a quarter of the window at four bytes a token, within the floor and the ceiling.
    pub fn for_window(window_tokens: Option<i64>) -> Self {
        let tokens = window_tokens
            .filter(|tokens| *tokens > 0)
            .unwrap_or(Self::UNKNOWN_WINDOW_TOKENS);
        let bytes = usize::try_from(tokens).unwrap_or(Self::CEILING);
        Self {
            bytes: bytes.clamp(Self::FLOOR, Self::CEILING),
        }
    }

    /// The budget for the agent named `agent`, from the window its registry entry gives.
    pub fn for_agent(agent: &str) -> Self {
        Self::for_window(
            crate::registry::get_agent_config(agent)
                .ok()
                .and_then(|config| config.context_window_size()),
        )
    }

    /// The most bytes of history, all included.
    pub fn bytes(self) -> usize {
        self.bytes
    }

    /// How large one page of the `conversation_context` tool may be: a tool result is part of
    /// the reader's own context.
    pub fn page_bytes(self) -> usize {
        self.bytes / 4
    }

    /// What typed (image and other non-text) history may take of it.
    pub fn attachment_bytes(self) -> usize {
        self.bytes / 4
    }
}

/// `text` within `max` bytes: when it is longer, its start and its end, and a note between them
/// of how many bytes were left out. `tail_share` is how much of `max` goes to the end, as a
/// number of eighths, because the newest part of a history is usually the part that matters.
/// Never splits a character.
pub fn clip(text: &str, max: usize, tail_share: usize) -> Cow<'_, str> {
    if text.len() <= max {
        return Cow::Borrowed(text);
    }
    let tail_len = max * tail_share.min(8) / 8;
    let head_len = max - tail_len;
    let mut head_end = head_len;
    while !text.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = text.len() - tail_len;
    while !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let omitted = tail_start.saturating_sub(head_end);
    Cow::Owned(format!(
        "{}[… {omitted} bytes omitted]{}",
        &text[..head_end],
        &text[tail_start..]
    ))
}

/// A summary within `max` bytes, as the last guard of every checkpoint. A summary is a story
/// told in order, so most of what is kept is its end. A trailing block of typed history (see
/// `fork_context`) is kept whole, unless it alone would not fit.
pub fn fit_summary(summary: &str, max: usize) -> Cow<'_, str> {
    if summary.len() <= max {
        return Cow::Borrowed(summary);
    }
    const TYPED: &str = "\n<nori-typed-history>";
    match summary.rsplit_once(TYPED) {
        Some((body, typed)) if typed.len() + TYPED.len() <= max / 2 => {
            let body = clip(body, max - typed.len() - TYPED.len(), 7);
            Cow::Owned(format!("{body}{TYPED}{typed}"))
        }
        Some((body, _)) => clip(body, max, 7),
        None => clip(summary, max, 7),
    }
}

const CHECKPOINT_OPEN: &str = "<conversation-checkpoint>";
const CHECKPOINT_CLOSE: &str = "</conversation-checkpoint>";

/// A built checkpoint cut to `max` bytes, keeping its frame and its newest end. `None` when it
/// already fits, so the caller knows a retry with it would send nothing smaller.
pub fn shrink_checkpoint(checkpoint: &str, max: usize) -> Option<String> {
    if checkpoint.len() <= max {
        return None;
    }
    let inner_start = checkpoint
        .find(CHECKPOINT_OPEN)
        .map_or(0, |at| at + CHECKPOINT_OPEN.len());
    let inner_end = checkpoint.rfind(CHECKPOINT_CLOSE)?;
    let inner = checkpoint.get(inner_start..inner_end)?;
    let frame = checkpoint.len() - inner.len();
    let clipped = clip(
        inner,
        max.saturating_sub(frame).max(ContextBudget::FLOOR),
        7,
    );
    let smaller = format!(
        "{}{clipped}{}",
        &checkpoint[..inner_start],
        &checkpoint[inner_end..]
    );
    (smaller.len() < checkpoint.len()).then_some(smaller)
}

/// Shrink the checkpoint a prompt opens with to `fraction` of its size, once a provider has
/// refused the prompt as too long (`fraction` is a step of the policy's `compaction.shrink_ladder`,
/// taken of the checkpoint as first built). Returns whether anything got smaller, that is, whether
/// a retry is worth making. Only Nori's own injected block is touched, never the user's words.
pub fn shrink_prompt_checkpoint(prompt: &mut [acp::ContentBlock], fraction: f64) -> bool {
    let mut shrunk = false;
    for block in prompt {
        let acp::ContentBlock::Text(text) = block else {
            continue;
        };
        let Some(start) = text.text.find(CHECKPOINT_OPEN) else {
            continue;
        };
        let Some(end) = text.text[start..]
            .find(CHECKPOINT_CLOSE)
            .map(|at| start + at + CHECKPOINT_CLOSE.len())
        else {
            continue;
        };
        let checkpoint = &text.text[start..end];
        let target = (checkpoint.len() as f64 * fraction) as usize;
        if let Some(smaller) = shrink_checkpoint(checkpoint, target) {
            text.text = format!("{}{smaller}{}", &text.text[..start], &text.text[end..]);
            shrunk = true;
        }
    }
    shrunk
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn the_budget_is_a_quarter_of_the_window_within_the_floor_and_the_ceiling() {
        assert_eq!(
            ContextBudget::for_window(Some(258_000)).bytes(),
            258_000,
            "a quarter of a window, at four bytes a token"
        );
        assert_eq!(
            ContextBudget::for_window(Some(2_000_000)).bytes(),
            ContextBudget::CEILING
        );
        assert_eq!(
            ContextBudget::for_window(Some(8_000)).bytes(),
            ContextBudget::FLOOR
        );
        assert_eq!(
            ContextBudget::for_window(None).bytes(),
            ContextBudget::UNKNOWN_WINDOW_TOKENS as usize
        );
        assert_eq!(
            ContextBudget::for_window(Some(-1)),
            ContextBudget::for_window(None)
        );
        // Well under the provider's 10 MiB string limit, whatever the window.
        assert!(ContextBudget::for_window(Some(i64::MAX)).bytes() <= 10 * 1024 * 1024 / 8);
    }

    #[test]
    fn a_clip_keeps_both_ends_says_what_it_cut_and_never_splits_a_character() {
        assert_eq!(clip("short", 100, 4), "short");
        let text = "é".repeat(1000);
        let clipped = clip(&text, 101, 4);
        assert!(clipped.contains("bytes omitted]"));
        assert!(clipped.len() < text.len());
        assert!(clipped.starts_with('é') && clipped.ends_with('é'));
        let long = format!("{}{}", "a".repeat(100), "z".repeat(100));
        let tail_heavy = clip(&long, 80, 7);
        assert!(tail_heavy.starts_with("aaaaaaaaaa[…"), "{tail_heavy}");
        assert!(tail_heavy.ends_with(&"z".repeat(70)));
    }

    #[test]
    fn a_summary_keeps_its_typed_history_and_its_newest_end() {
        let body = format!("start{}end", "x".repeat(10_000));
        let summary = format!("{body}\n<nori-typed-history>[1]</nori-typed-history>");
        let fitted = fit_summary(&summary, 1_000);
        assert!(fitted.len() <= 1_100, "{}", fitted.len());
        assert!(fitted.starts_with("start"));
        assert!(fitted.contains("end\n<nori-typed-history>[1]</nori-typed-history>"));
        assert_eq!(fit_summary("small", 1_000), "small");
        assert!(fit_summary("body\n<nori-typed-history>", 1).contains("bytes omitted"));
    }

    #[test]
    fn a_checkpoint_shrinks_inside_its_frame() {
        let checkpoint = format!(
            "{CHECKPOINT_OPEN}\nintro {} newest{CHECKPOINT_CLOSE}",
            "x".repeat(400_000)
        );
        let smaller = shrink_checkpoint(&checkpoint, 100_000).expect("shrunk");
        assert!(smaller.len() < 100_100, "{}", smaller.len());
        assert!(smaller.starts_with(CHECKPOINT_OPEN));
        assert!(smaller.ends_with(&format!("newest{CHECKPOINT_CLOSE}")));
        assert_eq!(shrink_checkpoint("tiny", 100_000), None);
    }

    #[test]
    fn a_refused_prompt_shrinks_only_the_injected_checkpoint() {
        let words = "what the user asked".to_owned();
        let mut prompt = vec![
            acp::ContentBlock::Text(acp::TextContent::new(format!(
                "{CHECKPOINT_OPEN}\n{}{CHECKPOINT_CLOSE}\n\nother context",
                "x".repeat(400_000)
            ))),
            acp::ContentBlock::Text(acp::TextContent::new(words.clone())),
        ];
        assert!(shrink_prompt_checkpoint(&mut prompt, 0.25));
        let acp::ContentBlock::Text(first) = &prompt[0] else {
            panic!("text block");
        };
        assert!(first.text.len() < 140_000, "{}", first.text.len());
        assert!(first.text.ends_with("\n\nother context"));
        assert_eq!(
            prompt[1],
            acp::ContentBlock::Text(acp::TextContent::new(words))
        );
        // A ladder step is a fraction of the checkpoint as built: 0.85 cuts less than 0.35.
        let built = |size: usize| {
            vec![acp::ContentBlock::Text(acp::TextContent::new(format!(
                "{CHECKPOINT_OPEN}\n{}{CHECKPOINT_CLOSE}",
                "x".repeat(size)
            )))]
        };
        let length = |prompt: &[acp::ContentBlock]| match &prompt[0] {
            acp::ContentBlock::Text(text) => text.text.len(),
            _ => 0,
        };
        let (mut gentle, mut harsh) = (built(400_000), built(400_000));
        assert!(shrink_prompt_checkpoint(&mut gentle, 0.85));
        assert!(shrink_prompt_checkpoint(&mut harsh, 0.35));
        assert!(
            length(&harsh) < length(&gentle),
            "{} {}",
            length(&harsh),
            length(&gentle)
        );
        assert!(length(&gentle) < 400_000 * 9 / 10);
        // Nothing left to shrink below the floor.
        let mut small = vec![acp::ContentBlock::Text(acp::TextContent::new(format!(
            "{CHECKPOINT_OPEN}\nsmall{CHECKPOINT_CLOSE}"
        )))];
        assert!(!shrink_prompt_checkpoint(&mut small, 0.25));
    }
}
