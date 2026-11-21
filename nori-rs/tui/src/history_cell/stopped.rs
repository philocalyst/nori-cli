//! The mark a turn leaves when it stops before finishing: one coral bar and the reason, with
//! `/continue` after it when the turn can be taken up. It takes the place of the notices that
//! used to say the same thing in two lines.

use ratatui::text::Line;
use ratatui::text::Span;

use super::HistoryCell;
use super::chrome::Chrome;

/// Why a turn stopped before it finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StopWhy {
    /// The user cancelled the turn.
    Interrupted,
    /// The agent reached its token limit.
    TokenLimit,
    /// The agent reached its limit on requests within one turn.
    TurnLimit,
    /// Anything else: a failure, a refusal.
    Other,
}

/// The one-row marker a stopped turn leaves in the transcript.
#[derive(Debug)]
pub(crate) struct StoppedTurnCell {
    pub(crate) reason: StopWhy,
    /// Whether `/continue` can pick the turn up where it stopped.
    pub(crate) can_continue: bool,
}

impl HistoryCell for StoppedTurnCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        let chrome = Chrome::live();
        let reason = match self.reason {
            StopWhy::Interrupted => "interrupted",
            StopWhy::TokenLimit => "stopped at the token limit",
            StopWhy::TurnLimit => "stopped at the turn limit",
            StopWhy::Other => "stopped",
        };
        let mut spans = vec![
            Span::styled("▌", chrome.coral()),
            Span::raw(" "),
            Span::styled(reason, chrome.faint()),
        ];
        if self.can_continue {
            spans.push(Span::styled(" · /continue", chrome.faint()));
        }
        vec![Line::from(spans)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(cell: &StoppedTurnCell) -> String {
        cell.display_lines(60)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn an_interrupted_turn_can_be_continued() {
        let cell = StoppedTurnCell {
            reason: StopWhy::Interrupted,
            can_continue: true,
        };
        insta::assert_snapshot!(text(&cell));
    }

    #[test]
    fn a_turn_stopped_at_the_token_limit_is_not_offered_for_continue() {
        let cell = StoppedTurnCell {
            reason: StopWhy::TokenLimit,
            can_continue: false,
        };
        insta::assert_snapshot!(text(&cell));
    }
}
