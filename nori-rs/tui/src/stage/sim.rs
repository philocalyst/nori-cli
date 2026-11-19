//! `StageSim`: the stage on an in-memory terminal, for checking what a real screen would show.
//!
//! The terminal is a `vt100` screen with scrollback ([`VT100Backend`]) that moves rows on a
//! resize the way a cursor-following terminal does (see [`VT100Backend::resize`]). The frames are
//! presented by the real [`Presenter`] (the same code the real `Tui` runs: plan, scroll or type,
//! cell grid, toasts, cursor), so a sequence of conversation changes, key presses and resizes can
//! be replayed and the screen, the scrollback and the cursor read back.
//!
//! The bottom block is a stand-in for the chat widget: a blank row, the composer (the draft, or
//! a placeholder), and a status row; a live tail (a streaming answer) sits above them, and a
//! picker can cover the whole region. The conversation is real cells, and focus is the real
//! [`FocusView`].
//!
//! ```ignore
//! let mut sim = StageSim::new(80, 24);
//! sim.say("MARK01");                // a prompt, and the answer
//! sim.resize(50, 20);
//! assert!(sim.problems(&["MARK01"]).is_empty());
//! println!("{}", sim.capture());
//! ```

#![allow(dead_code)]

use std::sync::Arc;

use ratatui::layout::Size;
use ratatui::style::Style;
use ratatui::text::Line;

use super::FocusView;
use super::StageView;
use crate::custom_terminal::Terminal;
use crate::history_cell::HistoryCell;
use crate::history_cell::PlainHistoryCell;
use crate::history_cell::UserHistoryCell;
use crate::test_backend::VT100Backend;
use crate::tui::present::CursorReport;
use crate::tui::present::Presenter;

pub(crate) struct StageSim {
    terminal: Terminal<VT100Backend>,
    presenter: Presenter,
    cells: Vec<Arc<dyn HistoryCell>>,
    focus: Option<FocusView>,
    draft: String,
    /// The streaming answer shown above the composer.
    live_tail: Vec<String>,
    /// A picker covering the region.
    picker: bool,
    /// Rows of a popup under the composer.
    popup_rows: u16,
    /// How the terminal behaves when it is resized.
    behavior: TerminalBehavior,
    /// What the cursor query after the last resize answered.
    report: Option<CursorReport>,
}

/// What the simulated terminal does that terminals differ on.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TerminalBehavior {
    /// A taller screen pulls rows back from the scrollback (iTerm2, kitty, Ghostty), instead of
    /// only adding blank rows at the bottom (xterm).
    pub(crate) pulls_on_grow: bool,
    /// The terminal answers the cursor-position query after a resize. A shorter screen pushes
    /// rows into the scrollback whatever it does.
    pub(crate) answers_cursor_query: bool,
}

impl Default for TerminalBehavior {
    fn default() -> Self {
        Self {
            pulls_on_grow: true,
            answers_cursor_query: true,
        }
    }
}

impl StageSim {
    /// A terminal of `cols` x `rows` with nothing on it, and the stage drawn once.
    pub(crate) fn new(cols: u16, rows: u16) -> Self {
        Self::after_shell(cols, rows, &[])
    }

    /// A terminal that shows `shell` rows (what a shell printed before Nori started) when the
    /// stage is first drawn under them.
    pub(crate) fn after_shell(cols: u16, rows: u16, shell: &[&str]) -> Self {
        let mut backend = VT100Backend::with_scrollback(cols, rows);
        for line in shell {
            std::io::Write::write_all(&mut backend, format!("{line}\r\n").as_bytes())
                .expect("write shell rows");
        }
        let terminal = Terminal::with_options(backend).expect("terminal");
        let mut sim = Self {
            terminal,
            presenter: Presenter::default(),
            cells: Vec::new(),
            focus: None,
            draft: String::new(),
            live_tail: Vec::new(),
            picker: false,
            popup_rows: 0,
            behavior: TerminalBehavior::default(),
            report: None,
        };
        sim.frame();
        sim
    }

    // --- the conversation ---------------------------------------------------------------------

    /// A prompt and its answer ("reply to <text>"), then a frame.
    pub(crate) fn say(&mut self, text: &str) {
        self.push(Arc::new(UserHistoryCell {
            message: text.to_owned(),
        }));
        self.push(plain(&[&format!("reply to {text}")]));
    }

    /// Add a cell and draw a frame.
    pub(crate) fn push(&mut self, cell: Arc<dyn HistoryCell>) {
        self.cells.push(cell);
        self.frame();
    }

    /// Show `rows` as the streaming answer above the composer.
    pub(crate) fn stream(&mut self, rows: &[&str]) {
        self.live_tail = rows.iter().map(|row| (*row).to_owned()).collect();
        self.frame();
    }

    /// The streaming answer ends: it becomes a cell.
    pub(crate) fn finish_stream(&mut self) {
        let rows: Vec<String> = std::mem::take(&mut self.live_tail);
        let rows: Vec<&str> = rows.iter().map(String::as_str).collect();
        self.push(plain(&rows));
    }

    // --- keys -----------------------------------------------------------------------------------

    /// Type `text` into the composer.
    pub(crate) fn type_text(&mut self, text: &str) {
        self.draft.push_str(text);
        self.frame();
    }

    /// Enter: the draft is sent as a prompt (answered at once).
    pub(crate) fn enter(&mut self) {
        let draft = std::mem::take(&mut self.draft);
        if !draft.is_empty() {
            self.say(&draft);
        }
    }

    /// Esc on an empty composer focuses the last prompt; Esc while focused leaves.
    pub(crate) fn escape(&mut self) {
        if self.focus.take().is_none()
            && let Some(selected) = self
                .cells
                .iter()
                .rposition(|cell| cell.as_any().is::<UserHistoryCell>())
        {
            let total = self
                .cells
                .iter()
                .filter(|cell| cell.as_any().is::<UserHistoryCell>())
                .count();
            self.focus = Some(FocusView {
                selected,
                turn: (total, total),
                edit: None,
                undo_turns: None,
                has_downstream: selected + 1 < self.cells.len(),
            });
        }
        self.frame();
    }

    pub(crate) fn open_picker(&mut self) {
        self.picker = true;
        self.frame();
    }

    pub(crate) fn close_picker(&mut self) {
        self.picker = false;
        self.presenter.full_repaint = true;
        self.frame();
    }

    /// `Ctrl+L`.
    pub(crate) fn clear_screen(&mut self) {
        self.presenter.stage.request_clear();
        self.frame();
    }

    pub(crate) fn popup(&mut self, rows: u16) {
        self.popup_rows = rows;
        self.frame();
    }

    // --- the terminal ---------------------------------------------------------------------------

    /// Resize the terminal as a window drag would, and draw the frame that follows.
    pub(crate) fn resize(&mut self, cols: u16, rows: u16) {
        self.terminal
            .backend_mut()
            .resize(cols, rows, self.behavior.pulls_on_grow);
        self.report = self.behavior.answers_cursor_query.then(|| CursorReport {
            screen: Size::new(cols, rows),
            row: self.cursor().0,
        });
        self.frame();
    }

    /// Change how the terminal behaves from the next resize on.
    pub(crate) fn behave(&mut self, behavior: TerminalBehavior) {
        self.behavior = behavior;
    }

    /// Draw one frame.
    pub(crate) fn frame(&mut self) {
        let resized = self.terminal.size().expect("size") != self.terminal.last_known_screen_size;
        self.presenter
            .note_screen(&mut self.terminal, resized, self.report.take())
            .expect("note screen");
        let screen = self.terminal.size().expect("size");
        let draft = self.draft.clone();
        let live_tail = self.live_tail.clone();
        let picker = self.picker;
        let popup = self.popup_rows;
        let live = u16::try_from(live_tail.len()).unwrap_or(u16::MAX);
        let block = if picker {
            u16::MAX
        } else {
            (live + 3 + popup).min(screen.height)
        };
        let view = StageView::new(&self.cells, self.focus.as_ref());
        self.presenter
            .present(&mut self.terminal, &view, block, popup, |frame| {
                let area = frame.area();
                let buf = &mut *frame.buffer;
                if picker {
                    for row in 0..area.height {
                        buf.set_string(
                            area.x,
                            area.y + row,
                            format!("PICKER row {row}"),
                            Style::default(),
                        );
                    }
                    return;
                }
                let mut y = area.y;
                for row in &live_tail {
                    buf.set_string(area.x, y, row, Style::default());
                    y += 1;
                }
                y += 1;
                let text = if draft.is_empty() {
                    "› Ask Nori to do anything".to_owned()
                } else {
                    format!("› {draft}")
                };
                buf.set_string(area.x, y, text, Style::default());
                buf.set_string(area.x, y + 1, " INS  Sim · model", Style::default());
                frame
                    .set_cursor_position((area.x + 2 + u16::try_from(draft.len()).unwrap_or(0), y));
            })
            .expect("present");
    }

    // --- reading it back ------------------------------------------------------------------------

    /// The rows of the screen.
    pub(crate) fn screen_rows(&self) -> Vec<String> {
        let size = self.terminal.size().expect("size");
        self.terminal
            .backend()
            .vt100()
            .screen()
            .rows(0, size.width)
            .collect()
    }

    /// The rows that scrolled off the top, oldest first.
    pub(crate) fn scrollback_rows(&self) -> Vec<String> {
        self.terminal
            .backend()
            .scrollback_rows()
            .iter()
            .map(|(text, _)| text.clone())
            .collect()
    }

    /// Scrollback and screen, oldest first.
    pub(crate) fn all_rows(&self) -> Vec<String> {
        let mut rows = self.scrollback_rows();
        rows.extend(self.screen_rows());
        rows
    }

    /// The cursor as `(row, column)` of the screen.
    pub(crate) fn cursor(&self) -> (u16, u16) {
        self.terminal.backend().vt100().screen().cursor_position()
    }

    /// How many rows of scrollback and screen contain `needle`.
    pub(crate) fn count(&self, needle: &str) -> usize {
        self.all_rows()
            .iter()
            .filter(|row| row.contains(needle))
            .count()
    }

    /// Everything that is wrong with the screen. Each of `marks` appears in the conversation
    /// as a prompt and as its answer (two rows, once each, wherever they are); the composer is
    /// on screen exactly once and the cursor is on it; no row of the history is on screen twice.
    pub(crate) fn problems(&self, marks: &[&str]) -> Vec<String> {
        let mut problems = Vec::new();
        let rows = self.all_rows();
        for mark in marks {
            let seen = rows.iter().filter(|row| row.contains(mark)).count();
            // A focused prompt is drawn from the cells, so the rows around it may be on screen
            // again where they are in the scrollback already.
            if seen != 2 && !self.picker && self.focus.is_none() {
                problems.push(format!("{mark} appears on {seen} rows, wanted 2"));
            }
        }
        if !self.picker {
            let screen = self.screen_rows();
            let composers = screen
                .iter()
                .filter(|row| {
                    row.contains("Ask Nori") || row.starts_with("› ") && row.contains(&self.draft)
                })
                .count();
            if composers == 0 {
                problems.push("the composer is not on screen".to_owned());
            }
            let status = screen.iter().filter(|row| row.contains("INS  Sim")).count();
            if status != 1 {
                problems.push(format!("the status row is on screen {status} times"));
            }
        }
        problems
    }

    /// The scrollback, a rule, and the screen: what a person scrolling and looking would see.
    pub(crate) fn capture(&self) -> String {
        let (row, column) = self.cursor();
        let mut out = String::new();
        for row_text in self.scrollback_rows() {
            out.push_str(row_text.trim_end());
            out.push('\n');
        }
        out.push_str(&"─".repeat(40));
        out.push_str(" screen\n");
        for row_text in self.screen_rows() {
            out.push_str(row_text.trim_end());
            out.push('\n');
        }
        out.push_str(&format!("cursor {row},{column}\n"));
        out
    }
}

fn plain(rows: &[&str]) -> Arc<dyn HistoryCell> {
    Arc::new(PlainHistoryCell::new(
        rows.iter()
            .map(|row| Line::from((*row).to_owned()))
            .collect(),
    ))
}

#[cfg(test)]
#[allow(clippy::print_stdout)]
mod tests {
    use super::*;

    fn marks(count: usize) -> Vec<String> {
        (1..=count).map(|n| format!("MARK{n:02}")).collect()
    }

    fn fill(sim: &mut StageSim, count: usize) {
        for mark in marks(count) {
            sim.say(&mark);
        }
    }

    fn check(sim: &StageSim, count: usize, label: &str) {
        let marks = marks(count);
        let marks: Vec<&str> = marks.iter().map(String::as_str).collect();
        let problems = sim.problems(&marks);
        println!(
            "== {label}\n{}scrollback rows {}, last mark on screen {}",
            sim.capture(),
            sim.scrollback_rows().len(),
            sim.screen_rows()
                .iter()
                .any(|row| row.contains(marks[marks.len() - 1])),
        );
        assert_eq!(problems, Vec::<String>::new(), "{label}");
    }

    /// The tail of the conversation is on screen, above the composer.
    fn assert_tail_visible(sim: &StageSim, mark: &str, label: &str) {
        let screen = sim.screen_rows();
        assert!(
            screen.iter().any(|row| row.contains(mark)),
            "{label}: {mark} is not on screen:\n{}",
            sim.capture()
        );
    }

    const RESIZES: [(u16, u16); 7] = [
        (40, 16),
        (80, 16),
        (80, 10),
        (80, 24),
        (30, 12),
        (60, 16),
        (100, 30),
    ];

    #[test]
    fn rows_are_committed_once_and_the_tail_stays_visible_across_resizes() {
        let mut sim = StageSim::after_shell(60, 16, &["~ ❯ nori"]);
        fill(&mut sim, 10);
        check(&sim, 10, "10 turns on 60x16");
        for (cols, rows) in RESIZES {
            sim.resize(cols, rows);
            let label = format!("resized to {cols}x{rows}");
            check(&sim, 10, &label);
            assert_tail_visible(&sim, "MARK10", &label);
        }
        sim.clear_screen();
        check(&sim, 10, "after Ctrl+L");
    }

    #[test]
    fn a_terminal_that_never_pulls_rows_back_loses_nothing() {
        let mut sim = StageSim::new(60, 16);
        sim.behave(TerminalBehavior {
            pulls_on_grow: false,
            answers_cursor_query: true,
        });
        fill(&mut sim, 10);
        for (cols, rows) in RESIZES {
            sim.resize(cols, rows);
            let label = format!("no pull, resized to {cols}x{rows}");
            check(&sim, 10, &label);
            assert_tail_visible(&sim, "MARK10", &label);
        }
    }

    #[test]
    fn a_terminal_that_does_not_answer_keeps_the_context_and_may_repeat_rows() {
        let mut sim = StageSim::new(60, 16);
        sim.behave(TerminalBehavior {
            pulls_on_grow: true,
            answers_cursor_query: false,
        });
        fill(&mut sim, 10);
        for (cols, rows) in RESIZES {
            sim.resize(cols, rows);
            let label = format!("silent terminal, resized to {cols}x{rows}");
            println!("== {label}\n{}", sim.capture());
            assert_tail_visible(&sim, "MARK10", &label);
            for mark in marks(10) {
                assert!(sim.count(&mark) >= 2, "{label}: {mark} was lost");
            }
            assert_eq!(
                sim.screen_rows()
                    .iter()
                    .filter(|row| row.contains("Ask Nori"))
                    .count(),
                1,
                "{label}: the composer"
            );
        }
    }

    #[test]
    fn resize_while_streaming_focused_and_in_a_picker() {
        let mut sim = StageSim::new(60, 14);
        fill(&mut sim, 6);
        sim.stream(&["streaming 1", "streaming 2", "streaming 3"]);
        sim.resize(40, 10);
        sim.resize(70, 18);
        sim.finish_stream();
        check(&sim, 6, "streamed");
        sim.escape();
        sim.resize(50, 12);
        check(&sim, 6, "focused, resized");
        sim.escape();
        sim.open_picker();
        sim.resize(40, 10);
        sim.close_picker();
        check(&sim, 6, "after picker");
    }
}
