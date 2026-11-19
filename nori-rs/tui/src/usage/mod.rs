//! The `/usage` overlay: what the agents spent, by provider and model, and how much quota is left.
//!
//! The overlay reads `$NORI_HOME/usage` through [`UsageStore::summary`] on a task, so opening it
//! or switching period never blocks the UI thread; the finished summary arrives on a channel the
//! overlay drains between events. See `docs/usage.md`.

mod accounts;
mod breakdown;
mod chart;
mod detail;
#[cfg(feature = "storybook")]
pub(crate) mod fixtures;
mod format;
mod header;
mod limits;
mod overview;
mod prices;
mod render;
mod shares;
#[cfg(test)]
mod tests;
mod totals;
mod view;

use std::path::Path;

use chrono::Utc;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use crossterm::event::MouseButton;
use crossterm::event::MouseEvent;
use crossterm::event::MouseEventKind;
use nori_harness::usage::UsagePeriod;
use nori_harness::usage::UsageStore;
use nori_harness::usage::UsageSummary;
use ratatui::buffer::Buffer;
use ratatui::layout::Position;
use ratatui::layout::Rect;
use tokio::sync::mpsc;

use self::header::Hit;
use self::header::PERIODS;
use self::render::Drawn;
use self::render::Frame;
use self::view::ModelKey;
use self::view::Screen;
use self::view::ranked_models;
use crate::tui;
use crate::tui::FrameRequester;
use crate::tui::TuiEvent;
pub use view::Metric;

const PAGE_ROWS: i32 = 8;
const WHEEL_ROWS: i32 = 3;

pub(crate) struct UsageOverlay {
    store: UsageStore,
    requester: FrameRequester,
    period: UsagePeriod,
    metric: Metric,
    /// The user picked the metric, so the data no longer decides it.
    metric_chosen: bool,
    /// Restrict every figure to one provider.
    provider: Option<String>,
    screen: Screen,
    /// The model row the cursor is on in the overview, if the cursor is in the table at all.
    selected: Option<ModelKey>,
    /// The newest summary to arrive, whichever provider is shown.
    loaded: Option<UsageSummary>,
    /// `loaded` as the provider filter leaves it.
    shown: Option<UsageSummary>,
    results_tx: mpsc::UnboundedSender<UsageSummary>,
    results_rx: mpsc::UnboundedReceiver<UsageSummary>,
    scroll: i32,
    /// The selection moved: scroll it into view on the next frame.
    follow_selection: bool,
    drawn: Option<Drawn>,
    done: bool,
}

impl UsageOverlay {
    /// Open on the last 7 days and start reading.
    pub(crate) fn open(nori_home: &Path, requester: FrameRequester) -> Self {
        let (results_tx, results_rx) = mpsc::unbounded_channel();
        let overlay = Self {
            store: UsageStore::new(nori_home),
            requester,
            period: UsagePeriod::Week,
            metric: Metric::Cost,
            metric_chosen: false,
            provider: None,
            screen: Screen::Overview,
            selected: None,
            loaded: None,
            shown: None,
            results_tx,
            results_rx,
            scroll: 0,
            follow_selection: false,
            drawn: None,
            done: false,
        };
        overlay.load();
        overlay
    }

    fn load(&self) {
        let store = self.store.clone();
        let period = self.period;
        let tx = self.results_tx.clone();
        let requester = self.requester.clone();
        tokio::spawn(async move {
            let summary = store.summary(period, Utc::now()).await;
            let _ = tx.send(summary);
            requester.schedule_frame();
        });
    }

    pub(crate) fn is_done(&self) -> bool {
        self.done
    }

    /// Recompute what is shown after the data or the provider filter changed.
    fn reshape(&mut self) {
        self.shown = self.loaded.as_ref().map(|summary| match &self.provider {
            Some(provider) => summary.only_provider(provider),
            None => summary.clone(),
        });
    }

    fn select_period(&mut self, period: UsagePeriod) {
        if self.period != period {
            self.period = period;
            self.scroll = 0;
            self.load();
        }
    }

    /// The provider filter steps through "all" and each provider in the loaded data.
    fn cycle_provider(&mut self) {
        let providers: Vec<&str> = self
            .loaded
            .iter()
            .flat_map(|summary| {
                summary
                    .providers
                    .iter()
                    .map(|usage| usage.provider.as_str())
            })
            .collect();
        let next = match &self.provider {
            None => providers.first().map(|provider| (*provider).to_owned()),
            Some(current) => providers
                .iter()
                .position(|provider| provider == current)
                .and_then(|index| providers.get(index + 1))
                .map(|provider| (*provider).to_owned()),
        };
        self.provider = next;
        self.selected = None;
        self.screen = Screen::Overview;
        self.scroll = 0;
        self.reshape();
    }

    fn toggle_prices(&mut self) {
        self.screen = match self.screen {
            Screen::Prices => Screen::Overview,
            Screen::Overview | Screen::Model(_) => Screen::Prices,
        };
        self.scroll = 0;
    }

    fn scroll_by(&mut self, rows: i32) {
        let max = self.drawn.as_ref().map_or(i32::MAX, |drawn| {
            (drawn.content_rows - drawn.viewport_rows).max(0)
        });
        self.scroll = (self.scroll + rows).clamp(0, max);
    }

    /// Move the cursor through the model table; above its first row the cursor leaves the table
    /// and the view scrolls.
    fn move_selection(&mut self, step: i32) {
        if self.screen != Screen::Overview {
            self.scroll_by(step);
            return;
        }
        let ranked: Vec<ModelKey> = self.shown.as_ref().map_or_else(Vec::new, |summary| {
            ranked_models(summary, self.metric)
                .iter()
                .map(|model| (model.provider.clone(), model.model.clone()))
                .collect()
        });
        let current = self
            .selected
            .as_ref()
            .and_then(|key| ranked.iter().position(|other| other == key));
        match (current, step < 0) {
            (None, true) => self.scroll_by(step),
            (None, false) => self.selected = ranked.first().cloned(),
            (Some(0), true) => {
                self.selected = None;
                self.scroll = 0;
            }
            (Some(index), up) => {
                let last = ranked.len().saturating_sub(1);
                let next = if up { index - 1 } else { (index + 1).min(last) };
                self.selected = ranked.get(next).cloned();
            }
        }
        self.follow_selection = self.selected.is_some();
    }

    fn open_selected(&mut self) {
        if self.screen == Screen::Overview
            && let Some(key) = self.selected.clone()
        {
            self.screen = Screen::Model(key);
            self.scroll = 0;
        }
    }

    fn back(&mut self) {
        match self.screen {
            Screen::Overview => self.done = true,
            Screen::Model(_) | Screen::Prices => {
                self.screen = Screen::Overview;
                self.follow_selection = self.selected.is_some();
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press && key.kind != KeyEventKind::Repeat {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.back(),
            KeyCode::Char('q') => self.done = true,
            KeyCode::Char('c') if ctrl => self.done = true,
            KeyCode::Enter => self.open_selected(),
            KeyCode::Char('d') => self.select_period(UsagePeriod::Day),
            KeyCode::Char('w') => self.select_period(UsagePeriod::Week),
            KeyCode::Char('m') => self.select_period(UsagePeriod::Month),
            KeyCode::Tab | KeyCode::BackTab => {
                let at = PERIODS.iter().position(|period| *period == self.period);
                let next = at.map_or(0, |index| (index + 1) % PERIODS.len());
                self.select_period(PERIODS[next]);
            }
            KeyCode::Char('p') => self.cycle_provider(),
            KeyCode::Char('r' | '$') => self.toggle_prices(),
            KeyCode::Char('t') => {
                self.metric = self.metric.toggled();
                self.metric_chosen = true;
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::PageUp => self.scroll_by(-PAGE_ROWS),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll_by(PAGE_ROWS),
            KeyCode::Home | KeyCode::Char('g') => self.scroll = 0,
            KeyCode::End | KeyCode::Char('G') => self.scroll_by(i32::MAX / 2),
            _ => {}
        }
    }

    fn on_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll_by(-WHEEL_ROWS),
            MouseEventKind::ScrollDown => self.scroll_by(WHEEL_ROWS),
            MouseEventKind::Down(MouseButton::Left) => {
                let position = Position::new(mouse.column, mouse.row);
                let hit = self.drawn.as_ref().and_then(|drawn| {
                    drawn
                        .hits
                        .iter()
                        .find(|(rect, _)| rect.contains(position))
                        .map(|(_, hit)| hit.clone())
                });
                match hit {
                    Some(Hit::Period(period)) => self.select_period(period),
                    Some(Hit::Metric(metric)) => {
                        self.metric = metric;
                        self.metric_chosen = true;
                    }
                    Some(Hit::Provider) => self.cycle_provider(),
                    Some(Hit::Prices) => self.toggle_prices(),
                    Some(Hit::Model(key)) => {
                        self.selected = Some(key);
                        self.open_selected();
                    }
                    None => {}
                }
            }
            _ => {}
        }
    }

    fn draw(&mut self, area: Rect, buf: &mut Buffer) -> Drawn {
        let theme = crate::style::component_theme();
        render::render(
            &Frame {
                theme: &theme,
                period: self.period,
                metric: self.metric,
                provider: self.provider.as_deref(),
                screen: &self.screen,
                selected: self.selected.as_ref(),
                summary: self.shown.as_ref(),
                now: Utc::now(),
                scroll: self.scroll,
            },
            area,
            buf,
        )
    }

    pub(crate) fn render(&mut self, area: Rect, buf: &mut Buffer) {
        while let Ok(summary) = self.results_rx.try_recv() {
            // A slower earlier request must not replace the period the user chose since.
            if summary.period == self.period {
                // Usage that has no prices has no cost to chart: open on tokens instead of a
                // flat zero, until the user chooses.
                if !self.metric_chosen
                    && summary.totals.cost_usd <= 0.0
                    && summary.totals.unpriced_tokens > 0
                {
                    self.metric = Metric::Tokens;
                }
                self.loaded = Some(summary);
                self.reshape();
            }
        }
        let mut drawn = self.draw(area, buf);
        if self.follow_selection {
            self.follow_selection = false;
            let row = self
                .selected
                .as_ref()
                .and_then(|key| drawn.model_rows.iter().find(|(_, other)| other == key))
                .map(|(row, _)| *row);
            if let Some(row) = row {
                let before = self.scroll;
                if row < self.scroll {
                    self.scroll = row;
                } else if row >= self.scroll + drawn.viewport_rows {
                    self.scroll = row - drawn.viewport_rows + 1;
                }
                if self.scroll != before {
                    drawn = self.draw(area, buf);
                }
            }
        }
        self.scroll = self
            .scroll
            .clamp(0, (drawn.content_rows - drawn.viewport_rows).max(0));
        self.drawn = Some(drawn);
    }

    pub(crate) fn handle_event(
        &mut self,
        tui: &mut tui::Tui,
        event: TuiEvent,
    ) -> std::io::Result<()> {
        match event {
            TuiEvent::Key(key) => {
                self.on_key(key);
                tui.frame_requester().schedule_frame();
            }
            TuiEvent::Mouse(mouse) => {
                self.on_mouse(mouse);
                tui.frame_requester().schedule_frame();
            }
            TuiEvent::Paste(_) => {}
            TuiEvent::Draw => {
                tui.draw_overlay(|frame| {
                    self.render(frame.area(), frame.buffer);
                })?;
            }
        }
        Ok(())
    }
}
