use std::fmt;
use std::io::IsTerminal;
use std::io::Result;
use std::io::stdin;
use std::io::stdout;
use std::panic;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use crossterm::Command;
use crossterm::event::DisableBracketedPaste;
use crossterm::event::DisableFocusChange;
use crossterm::event::EnableBracketedPaste;
use crossterm::event::EnableFocusChange;
use crossterm::event::Event;
use crossterm::event::KeyEvent;
use crossterm::event::KeyboardEnhancementFlags;
use crossterm::event::PopKeyboardEnhancementFlags;
use crossterm::event::PushKeyboardEnhancementFlags;
use crossterm::terminal::BeginSynchronizedUpdate;
use crossterm::terminal::EndSynchronizedUpdate;
use crossterm::terminal::supports_keyboard_enhancement;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::execute;
use ratatui::crossterm::queue;
use ratatui::crossterm::terminal::disable_raw_mode;
use ratatui::crossterm::terminal::enable_raw_mode;
use tokio::select;
use tokio_stream::Stream;

use crate::custom_terminal;
use crate::custom_terminal::Terminal as CustomTerminal;
use crate::frame_writer::FrameWriter;
use crate::stage::StageView;
#[cfg(unix)]
use crate::tui::job_control::SUSPEND_KEY;
#[cfg(unix)]
use crate::tui::job_control::SuspendContext;

#[cfg(unix)]
mod job_control;
pub(crate) mod present;
pub(crate) mod reflow;
mod repaint;
mod toast;

/// A type alias for the terminal type used in this application
pub type Terminal = CustomTerminal<CrosstermBackend<FrameWriter>>;

pub fn set_modes() -> Result<()> {
    execute!(stdout(), EnableBracketedPaste)?;

    enable_raw_mode()?;
    // Enable keyboard enhancement flags so modifiers for keys like Enter are disambiguated.
    // chat_composer.rs is using a keyboard event listener to enter for any modified keys
    // to create a new line that require this.
    // Some terminals (notably legacy Windows consoles) do not support
    // keyboard enhancement flags. Attempt to enable them, but continue
    // gracefully if unsupported.
    let _ = execute!(
        stdout(),
        PushKeyboardEnhancementFlags(keyboard_enhancement_flags())
    );

    let _ = execute!(stdout(), EnableFocusChange);
    Ok(())
}

fn keyboard_enhancement_flags() -> KeyboardEnhancementFlags {
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
        | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
}

/// Restore the terminal to its original state.
/// Inverse of `set_modes`.
pub fn restore() -> Result<()> {
    // Whatever this process told the terminal it is doing stops being true here.
    crate::terminal_status::retract_now();
    // Pop may fail on platforms that didn't support the push; ignore errors.
    let _ = execute!(stdout(), PopKeyboardEnhancementFlags);
    execute!(stdout(), DisableBracketedPaste)?;
    let _ = execute!(stdout(), DisableFocusChange);
    // Captured only while an inline interactive view requests mouse events.
    let _ = execute!(stdout(), crossterm::event::DisableMouseCapture);
    disable_raw_mode()?;
    let _ = execute!(
        stdout(),
        crossterm::cursor::SetCursorStyle::DefaultUserShape,
        crossterm::cursor::Show
    );
    Ok(())
}

/// Path to the controlling terminal, used for input when stdin is redirected.
#[cfg(unix)]
const CONTROLLING_TERMINAL: &str = "/dev/tty";
#[cfg(windows)]
const CONTROLLING_TERMINAL: &str = "CONIN$";

/// Initialize the terminal (inline viewport; history stays in normal scrollback)
pub fn init() -> Result<Terminal> {
    if !stdout().is_terminal() {
        return Err(std::io::Error::other("stdout is not a terminal"));
    }
    // stdin is allowed to be a pipe so `echo "prompt" | nori` can seed a session.
    // crossterm reads keys from the controlling terminal whenever stdin is not a
    // tty, so the only hard requirement is that one is actually attached.
    if !stdin().is_terminal() {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(CONTROLLING_TERMINAL)
            .map_err(|error| {
                std::io::Error::other(format!(
                    "stdin is not a terminal and no controlling terminal is available: {error}"
                ))
            })?;
    }
    set_modes()?;
    crate::startup_trace::mark("terminal_modes");
    crate::terminal_status::init();
    crate::startup_trace::mark("terminal_status");

    set_panic_hook();

    let backend = CrosstermBackend::new(FrameWriter::stdout()?);
    let tui = CustomTerminal::with_options(backend)?;
    crate::startup_trace::mark("cursor_position");
    Ok(tui)
}

/// Names the terminal for the saved colors: another terminal has other colors.
fn terminal_id() -> String {
    std::env::var("TERM_PROGRAM")
        .unwrap_or_else(|_| "default".to_owned())
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect()
}

fn set_panic_hook() {
    let hook = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        let _ = restore(); // ignore any errors as we are already failing
        hook(panic_info);
    }));
}

#[derive(Debug)]
pub enum TuiEvent {
    Key(KeyEvent),
    Paste(String),
    /// Clicks and the wheel, reported while mouse capture is enabled.
    Mouse(crossterm::event::MouseEvent),
    Draw,
}

pub struct Tui {
    frame_schedule_tx: tokio::sync::mpsc::UnboundedSender<Instant>,
    draw_tx: tokio::sync::broadcast::Sender<()>,
    pub(crate) terminal: Terminal,
    presenter: present::Presenter,
    /// Any resize can damage terminal contents, even if a burst ends at the original size.
    resize_signal: Arc<present::ResizeSignal>,
    /// Since when frames have waited for the resize event of a screen that changed size.
    resize_wait: Option<Instant>,
    /// Whether a frame was drawn yet, for the startup trace.
    first_frame_drawn: bool,
    #[cfg(unix)]
    suspend_context: SuspendContext,
    // True when terminal/tab is focused; updated internally from crossterm events
    terminal_focused: Arc<AtomicBool>,
    enhanced_keys_supported: bool,
    mouse_capture: bool,
    cursor_style: Option<crossterm::cursor::SetCursorStyle>,
    /// The conversation an open overlay is drawn under: it stays on screen above the panel.
    backdrop: Backdrop,
}

/// The conversation as the app last handed it over, for frames that an overlay draws.
#[derive(Default)]
struct Backdrop {
    cells: Vec<Arc<dyn crate::history_cell::HistoryCell>>,
}

/// How many rows a panel (a picker, the actor panel, a child's conversation, `/usage`) takes from
/// the bottom of a screen of `screen_height` rows. A panel never takes the whole screen when the
/// screen has room for the conversation too: what is above it stays where it is.
pub(crate) fn panel_height(screen_height: u16) -> u16 {
    if screen_height <= 16 {
        screen_height
    } else {
        (screen_height * 3 / 5).max(14)
    }
}

#[derive(Clone, Debug)]
pub struct FrameRequester {
    frame_schedule_tx: tokio::sync::mpsc::UnboundedSender<Instant>,
}
impl FrameRequester {
    pub fn schedule_frame(&self) {
        let _ = self.frame_schedule_tx.send(Instant::now());
    }
    pub fn schedule_frame_in(&self, dur: Duration) {
        let _ = self.frame_schedule_tx.send(Instant::now() + dur);
    }
}

#[cfg(test)]
impl FrameRequester {
    /// Create a no-op frame requester for tests.
    pub(crate) fn test_dummy() -> Self {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        FrameRequester {
            frame_schedule_tx: tx,
        }
    }
}

impl Tui {
    pub fn new(terminal: Terminal) -> Self {
        let (frame_schedule_tx, frame_schedule_rx) = tokio::sync::mpsc::unbounded_channel();
        let (draw_tx, _) = tokio::sync::broadcast::channel(1);

        spawn_frame_scheduler(frame_schedule_rx, draw_tx.clone());

        // Detect keyboard enhancement support before any EventStream is created so the
        // crossterm poller can acquire its lock without contention.
        let enhanced_keys_supported = supports_keyboard_enhancement().unwrap_or(false);
        crate::startup_trace::mark("keyboard_query");
        // Cache this to avoid contention with the event reader.
        supports_color::on_cached(supports_color::Stream::Stdout);
        // The terminal's colors come from the last run's answer; asking a new tab can take
        // seconds, so the question is put later, off the frame path.
        if let Ok(home) = nori_config::find_nori_home() {
            crate::terminal_palette::load_cached_colors(
                &home.join(format!(".terminal-colors-{}", terminal_id())),
            );
        }
        crate::startup_trace::mark("color_query");

        Self {
            frame_schedule_tx,
            draw_tx,
            terminal,
            presenter: present::Presenter {
                width_change: reflow::WidthChange::of_terminal(|name| std::env::var(name).ok()),
                ..present::Presenter::default()
            },
            resize_signal: Arc::new(present::ResizeSignal::default()),
            resize_wait: None,
            first_frame_drawn: false,
            #[cfg(unix)]
            suspend_context: SuspendContext::new(),
            terminal_focused: Arc::new(AtomicBool::new(true)),
            enhanced_keys_supported,
            mouse_capture: false,
            cursor_style: None,
            backdrop: Backdrop::default(),
        }
    }

    /// Send the terminal status reports that are waiting, when no frame is about to carry them.
    pub(crate) fn flush_status(&mut self) {
        if crate::terminal_status::has_pending() {
            let _ = std::io::Write::flush(self.terminal.backend_mut().writer_mut());
        }
    }

    /// The conversation an overlay's frames show above it.
    pub(crate) fn set_backdrop(&mut self, cells: &[Arc<dyn crate::history_cell::HistoryCell>]) {
        self.backdrop.cells.clear();
        self.backdrop.cells.extend_from_slice(cells);
    }

    pub fn frame_requester(&self) -> FrameRequester {
        FrameRequester {
            frame_schedule_tx: self.frame_schedule_tx.clone(),
        }
    }

    pub fn enhanced_keys_supported(&self) -> bool {
        self.enhanced_keys_supported
    }

    /// Emit a desktop notification now if the terminal is unfocused.
    /// Returns true if a notification was posted.
    pub fn notify(&mut self, message: impl AsRef<str>) -> bool {
        if !self.terminal_focused.load(Ordering::Relaxed) {
            let _ = execute!(stdout(), PostNotification(message.as_ref().to_string()));
            true
        } else {
            false
        }
    }

    pub fn event_stream(&self) -> Pin<Box<dyn Stream<Item = TuiEvent> + Send + 'static>> {
        use tokio_stream::StreamExt;

        let mut crossterm_events = crossterm::event::EventStream::new();
        let mut draw_rx = self.draw_tx.subscribe();
        let resize_frame = self.frame_requester();
        let resize_signal = self.resize_signal.clone();

        // State for tracking how we should resume from ^Z suspend.
        #[cfg(unix)]
        let suspend_context = self.suspend_context.clone();

        let terminal_focused = self.terminal_focused.clone();
        let event_stream = async_stream::stream! {
            // The terminal is asked for its colors once it has had time to settle; a new tab can
            // be slow to answer for its first seconds. It is asked from here, since a query
            // made from another thread loses its reply to this stream's reader.
            let palette_refresh = tokio::time::sleep(crate::terminal_palette::REFRESH_DELAY);
            tokio::pin!(palette_refresh);
            let mut palette_refreshed = false;
            // Unanswered cursor queries in a row; a terminal that never answers is not asked again.
            let mut unanswered = 0;
            loop {
                select! {
                    () = &mut palette_refresh, if !palette_refreshed => {
                        palette_refreshed = true;
                        if crate::terminal_palette::refresh_colors() {
                            yield TuiEvent::Draw;
                        }
                    }
                    Some(Ok(event)) = crossterm_events.next() => {
                        match event {
                            Event::Key(key_event) => {
                                #[cfg(unix)]
                                if SUSPEND_KEY.is_press(key_event) {
                                    let _ = suspend_context.suspend();
                                    // We continue here after resume.
                                    yield TuiEvent::Draw;
                                    continue;
                                }
                                yield TuiEvent::Key(key_event);
                            }
                            Event::Resize(_, _) => {
                                // The scheduler coalesces bursts; draw reads the latest PTY size.
                                // Where the terminal left the cursor says how it moved the screen;
                                // it can only be asked here, between reads of the input. One slow
                                // answer in a burst of resizes is no reason to stop asking; three
                                // in a row are.
                                let report = if unanswered < 3 {
                                    present::CursorReport::query()
                                } else {
                                    None
                                };
                                unanswered = if report.is_some() { 0 } else { unanswered + 1 };
                                resize_signal.note(report);
                                resize_frame.schedule_frame();
                            }
                            Event::Paste(pasted) => {
                                yield TuiEvent::Paste(pasted);
                            }
                            Event::Mouse(mouse) => {
                                yield TuiEvent::Mouse(mouse);
                            }
                            Event::FocusGained => {
                                terminal_focused.store(true, Ordering::Relaxed);
                                crate::terminal_palette::requery_default_colors();
                                yield TuiEvent::Draw;
                            }
                            Event::FocusLost => {
                                terminal_focused.store(false, Ordering::Relaxed);
                            }
                        }
                    }
                    result = draw_rx.recv() => {
                        match result {
                            Ok(_) => {
                                yield TuiEvent::Draw;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                // We dropped one or more draw notifications; coalesce to a single draw.
                                yield TuiEvent::Draw;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                // Sender dropped; stop emitting draws from this source.
                            }
                        }
                    }
                }
            }
        };
        Box::pin(event_stream)
    }

    /// Set the caret shape for the active editor or restore the terminal default.
    pub(crate) fn set_cursor_style(
        &mut self,
        style: crossterm::cursor::SetCursorStyle,
    ) -> Result<()> {
        if self.cursor_style == Some(style) {
            return Ok(());
        }
        self.terminal.backend_mut().writer_mut().begin_frame();
        queue!(self.terminal.backend_mut(), style)?;
        self.terminal.backend_mut().writer_mut().end_frame()?;
        self.cursor_style = Some(style);
        Ok(())
    }

    /// Report clicks and the wheel as [`TuiEvent::Mouse`] while `capture` is set. The terminal
    /// then selects text only with a modifier (Shift on most).
    pub fn set_mouse_capture(&mut self, capture: bool) -> Result<()> {
        if self.mouse_capture == capture {
            return Ok(());
        }
        self.terminal.backend_mut().writer_mut().begin_frame();
        let _ = if capture {
            queue!(
                self.terminal.backend_mut(),
                crossterm::event::EnableMouseCapture
            )
        } else {
            queue!(
                self.terminal.backend_mut(),
                crossterm::event::DisableMouseCapture
            )
        };
        self.terminal.backend_mut().writer_mut().end_frame()?;
        self.mouse_capture = capture;
        Ok(())
    }

    /// Show `text` as a toast in the top right for a few seconds.
    pub(crate) fn show_toast(&mut self, text: String) {
        self.presenter.toasts.push(text, Instant::now());
        let frames = self.frame_requester();
        frames.schedule_frame();
        // One more frame when it is time for it to go, to erase it.
        frames.schedule_frame_in(toast::TOAST_LIFETIME + Duration::from_millis(50));
    }

    /// Start the stage at `cell` of the conversation and type `note` where it begins, saying what
    /// was left out above it.
    pub(crate) fn start_stage_at(
        &mut self,
        cell: usize,
        note: ratatui::text::Line<'static>,
    ) -> Result<()> {
        self.presenter.stage.start_at(cell);
        let screen = self.terminal.size()?;
        let top = self.terminal.viewport_area.y;
        let backend = self.terminal.backend_mut();
        backend.writer_mut().begin_frame();
        crate::stage::write::type_rows(backend, &[note], top, screen.width)?;
        let top = if top + 1 < screen.height {
            top + 1
        } else {
            crate::stage::write::scroll_screen_up(backend, screen.height, 1)?;
            top
        };
        self.terminal.backend_mut().writer_mut().end_frame()?;
        self.terminal.set_viewport_area(ratatui::layout::Rect::new(
            0,
            top,
            screen.width,
            screen.height - top,
        ));
        self.terminal.invalidate();
        self.frame_requester().schedule_frame();
        Ok(())
    }

    /// The conversation lost every cell from `len` on.
    pub(crate) fn stage_truncated(&mut self, len: usize) {
        self.presenter.stage.truncated(len);
        self.frame_requester().schedule_frame();
    }

    /// The cells of `range` became `new_len` cells.
    pub(crate) fn stage_replaced(&mut self, range: std::ops::Range<usize>, new_len: usize) {
        self.presenter.stage.replaced(range, new_len);
        self.frame_requester().schedule_frame();
    }

    /// The expandable card (a tool, a peer message) of `cells` nearest the composer on screen.
    pub(crate) fn last_card(
        &self,
        cells: &[Arc<dyn crate::history_cell::HistoryCell>],
    ) -> Option<usize> {
        self.presenter.stage.last_card(&StageView::new(cells, None))
    }

    /// The expandable card drawn at `row` of the screen.
    pub(crate) fn card_at(&self, row: u16) -> Option<usize> {
        self.presenter.stage.card_at(row)
    }

    /// Draw a screen that is not the conversation (onboarding, a prompt): `draw_fn` gets the rows
    /// the region has, up to `height`.
    pub fn draw(
        &mut self,
        height: u16,
        popup_rows: u16,
        draw_fn: impl FnOnce(&mut custom_terminal::Frame),
    ) -> Result<()> {
        self.draw_stage(&StageView::empty(), height, popup_rows, draw_fn)
    }

    /// Draw a panel (a picker, the pager, the actor panel) at the bottom of the screen, under the
    /// conversation. The panel is transient: it clips the conversation from the top instead of
    /// pushing it into the scrollback, so closing it leaves the screen as it was.
    pub fn draw_overlay(
        &mut self,
        draw_fn: impl FnOnce(&mut custom_terminal::Frame),
    ) -> Result<()> {
        let height = panel_height(self.terminal.size()?.height);
        let cells = std::mem::take(&mut self.backdrop.cells);
        let result = self.draw_stage(&StageView::new(&cells, None), height, height, draw_fn);
        self.backdrop.cells = cells;
        result
    }

    /// Draw the conversation (`view`) with a bottom block of `height` rows, of which `popup_rows`
    /// are a popup. `draw_fn` draws the block in the area its frame has.
    pub fn draw_stage(
        &mut self,
        view: &StageView<'_>,
        height: u16,
        popup_rows: u16,
        draw_fn: impl FnOnce(&mut custom_terminal::Frame),
    ) -> Result<()> {
        let (resize_event, cursor_report) = self.resize_signal.take();
        // A screen that changed size is found again from where the terminal says the cursor went
        // after its last resize. A frame that comes before that answer (the terminal resized but
        // the event has not been read, or a later resize is still to be answered) would have to
        // guess, and a wrong guess draws again rows the terminal already pushed into the
        // scrollback. It waits for the answer instead, briefly: a terminal that does not answer
        // gets the guess.
        let screen = self.terminal.size()?;
        let moved = resize_event || screen != self.terminal.last_known_screen_size;
        let answered = cursor_report.is_some_and(|report| report.screen == screen);
        if moved && !answered {
            let waited = self.resize_wait.get_or_insert_with(Instant::now).elapsed();
            if waited < RESIZE_ANSWER_WAIT {
                if resize_event {
                    self.resize_signal.note(cursor_report);
                }
                self.frame_requester()
                    .schedule_frame_in(RESIZE_ANSWER_WAIT - waited);
                return Ok(());
            }
        }
        self.resize_wait = None;
        // If we are resuming from ^Z, we need to prepare the resume action now so we can apply it
        // in the synchronized update.
        #[cfg(unix)]
        let mut prepared_resume = self
            .suspend_context
            .prepare_resume_action(&mut self.terminal);
        // A resize is dealt with before the synchronized update: the screen is found again, and
        // nothing of the frame is drawn on a screen that moved.
        self.presenter
            .note_screen(&mut self.terminal, resize_event, cursor_report)?;

        // The whole frame, including the synchronized-update markers, is collected and written
        // with one syscall. The markers are closed and the buffer is sent on the error path too,
        // so a failed frame cannot leave the terminal inside a synchronized update.
        self.terminal.backend_mut().writer_mut().begin_frame();
        let _ = queue!(self.terminal.backend_mut(), BeginSynchronizedUpdate);
        let result = self.draw_frame(
            view,
            height,
            popup_rows,
            draw_fn,
            #[cfg(unix)]
            prepared_resume.take(),
        );
        let _ = queue!(self.terminal.backend_mut(), EndSynchronizedUpdate);
        let sent = self.terminal.backend_mut().writer_mut().end_frame();
        if !self.first_frame_drawn {
            self.first_frame_drawn = true;
            crate::startup_trace::mark("first_frame");
        }
        result.and(sent)
    }

    fn draw_frame(
        &mut self,
        view: &StageView<'_>,
        height: u16,
        popup_rows: u16,
        draw_fn: impl FnOnce(&mut custom_terminal::Frame),
        #[cfg(unix)] prepared_resume: Option<job_control::PreparedResumeAction>,
    ) -> Result<()> {
        if self.presenter.full_repaint {
            self.cursor_style = None;
        }
        #[cfg(unix)]
        if let Some(prepared) = prepared_resume {
            prepared.apply(&mut self.terminal)?;
            // What was drawn is above the shell's own output now: it is history, not tail.
            self.presenter.stage.release_tail();
            self.cursor_style = None;
        }
        self.presenter
            .present(&mut self.terminal, view, height, popup_rows, draw_fn)?;
        if let Some(next) = self.presenter.toast_frame() {
            self.frame_requester().schedule_frame_in(next);
        }
        #[cfg(unix)]
        self.suspend_context.set_cursor_y(
            self.presenter
                .block_top()
                .saturating_add(height)
                .saturating_sub(1),
        );
        Ok(())
    }

    /// Leave the screen for the shell: the bottom block is erased and the cursor goes to where it
    /// began, under the last row of the conversation. The conversation stays where it is.
    pub(crate) fn finish_screen(&mut self) -> Result<()> {
        let backend = self.terminal.backend_mut();
        backend.writer_mut().begin_frame();
        let result = self.presenter.finish(&mut self.terminal);
        self.terminal.backend_mut().writer_mut().end_frame()?;
        result
    }
}

/// How long frames wait for the cursor report of a resize before guessing where the screen went.
const RESIZE_ANSWER_WAIT: Duration = Duration::from_millis(250);

/// The shortest time between two draws. Requests inside the window are deferred to its end,
/// never dropped, so a burst of events costs one frame instead of one per event.
const MIN_FRAME_INTERVAL: Duration = Duration::from_millis(8);

/// When the next draw may fire, given the earliest requested time and when the last one fired.
fn clamp_to_frame_interval(requested: Instant, last_draw: Option<Instant>) -> Instant {
    match last_draw {
        Some(last_draw) => requested.max(last_draw + MIN_FRAME_INTERVAL),
        None => requested,
    }
}

/// Spawn background scheduler to coalesce frame requests and emit draws at deadlines.
fn spawn_frame_scheduler(
    frame_schedule_rx: tokio::sync::mpsc::UnboundedReceiver<Instant>,
    draw_tx: tokio::sync::broadcast::Sender<()>,
) {
    tokio::spawn(async move {
        use tokio::select;
        use tokio::time::Instant as TokioInstant;
        use tokio::time::sleep_until;

        let mut rx = frame_schedule_rx;
        let mut next_deadline: Option<Instant> = None;
        let mut last_draw: Option<Instant> = None;

        loop {
            let target = next_deadline
                .map(|requested| clamp_to_frame_interval(requested, last_draw))
                .unwrap_or_else(|| Instant::now() + Duration::from_secs(60 * 60 * 24 * 365));
            let sleep_fut = sleep_until(TokioInstant::from_std(target));
            tokio::pin!(sleep_fut);

            select! {
                recv = rx.recv() => {
                    match recv {
                        Some(at) => {
                            if next_deadline.is_none_or(|cur| at < cur) {
                                next_deadline = Some(at);
                            }
                            // Do not send a draw immediately here. By continuing the loop,
                            // we recompute the sleep target so the draw fires once via the
                            // sleep branch, coalescing multiple requests into a single draw.
                            continue;
                        }
                        None => break,
                    }
                }
                _ = &mut sleep_fut => {
                    if next_deadline.is_some() {
                        next_deadline = None;
                        last_draw = Some(Instant::now());
                        let _ = draw_tx.send(());
                    }
                }
            }
        }
    });
}

/// Command that emits an OSC 9 desktop notification with a message.
#[derive(Debug, Clone)]
pub struct PostNotification(pub String);

impl Command for PostNotification {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        write!(f, "\x1b]9;{}\x07", self.0)
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> Result<()> {
        Err(std::io::Error::other(
            "tried to execute PostNotification using WinAPI; use ANSI instead",
        ))
    }

    #[cfg(windows)]
    fn is_ansi_code_supported(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn a_request_inside_the_window_is_deferred_to_its_end() {
        let last = Instant::now();
        let requested = last + Duration::from_millis(2);
        assert_eq!(
            clamp_to_frame_interval(requested, Some(last)),
            last + MIN_FRAME_INTERVAL
        );
    }

    #[test]
    fn a_request_after_the_window_is_not_delayed() {
        let last = Instant::now();
        let requested = last + Duration::from_millis(50);
        assert_eq!(clamp_to_frame_interval(requested, Some(last)), requested);
    }

    #[test]
    fn the_first_draw_is_not_delayed() {
        let requested = Instant::now();
        assert_eq!(clamp_to_frame_interval(requested, None), requested);
    }

    #[tokio::test]
    async fn a_burst_of_requests_draws_once_per_window() {
        let (frame_tx, frame_rx) = tokio::sync::mpsc::unbounded_channel();
        let (draw_tx, mut draw_rx) = tokio::sync::broadcast::channel(16);
        spawn_frame_scheduler(frame_rx, draw_tx);

        // A request every millisecond.
        let started = Instant::now();
        let mut draws = 0;
        for _ in 0..100 {
            let _ = frame_tx.send(Instant::now());
            tokio::time::sleep(Duration::from_millis(1)).await;
            while draw_rx.try_recv().is_ok() {
                draws += 1;
            }
        }
        // One draw per window at most (the last one may still be pending), and at least one.
        let windows = started.elapsed().as_millis() / MIN_FRAME_INTERVAL.as_millis() + 1;
        assert!(
            draws >= 1 && draws <= windows,
            "{draws} draws in {windows} windows"
        );
    }
}
