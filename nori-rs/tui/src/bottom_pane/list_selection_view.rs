use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use nori_tui_components::Field;
use nori_tui_components::HelixConfig;
use nori_tui_components::HelixEvent;
use nori_tui_components::HelixItem;
use nori_tui_components::HelixState;
use nori_tui_components::KeyHint;
use nori_tui_components::MenuDensity;
use nori_tui_components::MenuItemTone;
use nori_tui_components::MenuPlacement;
use nori_tui_components::MenuRowPattern;
use nori_tui_components::RowContext;
use ratatui::buffer::Buffer;
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Block;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;

use crate::app_event_sender::AppEventSender;
use crate::key_hint;
use crate::key_hint::KeyBinding;
use crate::render::Insets;
use crate::render::RectExt as _;
use crate::render::renderable::ColumnRenderable;
use crate::render::renderable::Renderable;
use crate::style::user_message_style;

use super::CancellationEvent;
use super::bottom_pane_view::BottomPaneView;
use super::popup_consts::MAX_POPUP_ROWS;
use super::scroll_state::ScrollState;
use super::selection_popup_common::GenericDisplayRow;
use super::selection_popup_common::measure_rows_height;
use super::selection_popup_common::render_rows;

/// One selectable item in the generic selection list.
pub(crate) type SelectionAction = Box<dyn Fn(&AppEventSender) + Send + Sync>;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum SelectionPresentation {
    #[default]
    List,
    Picker,
    Menu,
}

#[derive(Default)]
pub(crate) struct SelectionItem {
    pub name: String,
    pub display_shortcut: Option<KeyBinding>,
    pub description: Option<String>,
    pub selected_description: Option<String>,
    pub is_current: bool,
    pub actions: Vec<SelectionAction>,
    pub dismiss_on_select: bool,
    pub search_value: Option<String>,
    /// When true, this row is a non-selectable section header: navigation skips
    /// it, it shows no number or cursor, and it renders as a bold section label.
    pub is_header: bool,
    pub menu_tone: MenuItemTone,
}

pub(crate) struct SelectionViewParams {
    pub title: Option<String>,
    pub subtitle: Option<String>,
    pub footer_hint: Option<Line<'static>>,
    pub footer_hint_right: Option<Line<'static>>,
    pub picker_footer_hints: Option<Vec<KeyHint<'static>>>,
    pub items: Vec<SelectionItem>,
    pub is_searchable: bool,
    pub search_placeholder: Option<String>,
    pub header: Box<dyn Renderable>,
    pub initial_selected_idx: Option<usize>,
    /// Optional callback fired when the picker is dismissed without selection
    /// (e.g. via Escape or Ctrl-C).
    pub on_dismiss: Option<SelectionAction>,
    /// Optional callback fired when Shift-Tab is pressed while the picker is open.
    pub on_shift_tab: Option<SelectionAction>,
    pub presentation: SelectionPresentation,
    pub menu_max_width: u16,
    pub menu_density: MenuDensity,
    pub menu_row_pattern: MenuRowPattern,
    pub menu_placement: MenuPlacement,
}

impl SelectionViewParams {
    pub(crate) fn picker(mut self) -> Self {
        self.presentation = SelectionPresentation::Picker;
        self
    }

    pub(crate) fn menu(
        mut self,
        max_width: u16,
        density: MenuDensity,
        row_pattern: MenuRowPattern,
        placement: MenuPlacement,
    ) -> Self {
        self.presentation = SelectionPresentation::Menu;
        self.menu_max_width = max_width;
        self.menu_density = density;
        self.menu_row_pattern = row_pattern;
        self.menu_placement = placement;
        self
    }
}

impl Default for SelectionViewParams {
    fn default() -> Self {
        Self {
            title: None,
            subtitle: None,
            footer_hint: None,
            footer_hint_right: None,
            picker_footer_hints: None,
            items: Vec::new(),
            is_searchable: false,
            search_placeholder: None,
            header: Box::new(()),
            initial_selected_idx: None,
            on_dismiss: None,
            on_shift_tab: None,
            presentation: SelectionPresentation::List,
            menu_max_width: 58,
            menu_density: MenuDensity::Normal,
            menu_row_pattern: MenuRowPattern::Plain,
            menu_placement: MenuPlacement::Centered,
        }
    }
}

/// A selection row for the engine: what it filters on, and what the painter needs.
struct Row {
    /// Position in the items the view was built from; the engine's identity for the row, and
    /// what [`ListSelectionView::take_last_selected_index`] reports.
    index: usize,
    name: String,
    description: Option<String>,
    selected_description: Option<String>,
    is_current: bool,
    display_shortcut: Option<KeyBinding>,
    /// What a query is matched against, and the stable id of `update_item`.
    search_value: Option<String>,
    section: Option<String>,
}

impl HelixItem for Row {
    type Id = usize;

    fn id(&self) -> usize {
        self.index
    }

    fn fields<'a>(&'a self, out: &mut Vec<Field<'a>>) {
        // A row with no search value cannot be found by a query.
        if let Some(value) = &self.search_value {
            out.push(Field::new("title", value, 0));
        }
    }

    fn section(&self) -> Option<&str> {
        self.section.as_deref()
    }

    fn rows(&self, _cx: &RowContext<'_>) -> Vec<Line<'static>> {
        // Painted by `render_rows`, not by the engine.
        Vec::new()
    }
}

/// The selection list shown in the bottom pane. The engine ([`HelixState`]) is its model:
/// filtering, selection, sections, digit and `/` modes. This type paints the rows the way every
/// bottom-pane popup does (`render_rows`) and carries what a bottom-pane view carries: header,
/// footer hints, dismiss and Shift-Tab actions.
pub(crate) struct ListSelectionView {
    footer_hint: Option<Line<'static>>,
    footer_hint_right: Option<Line<'static>>,
    model: HelixState<Row>,
    /// What each row does when chosen, by `Row::index`; header slots hold `None`.
    actions: Vec<Option<(Vec<SelectionAction>, bool)>>,
    /// Scroll window of the painted rows (section headings count as rows).
    scroll: ScrollState,
    complete: bool,
    app_event_tx: AppEventSender,
    is_searchable: bool,
    search_placeholder: Option<String>,
    last_selected_actual_idx: Option<usize>,
    header: Box<dyn Renderable>,
    on_dismiss: Option<SelectionAction>,
    on_shift_tab: Option<SelectionAction>,
}

impl ListSelectionView {
    pub fn new(params: SelectionViewParams, app_event_tx: AppEventSender) -> Self {
        let mut header = params.header;
        if params.title.is_some() || params.subtitle.is_some() {
            let title = params.title.map(|title| Line::from(title.bold()));
            let subtitle = params.subtitle.map(|subtitle| Line::from(subtitle.dim()));
            header = Box::new(ColumnRenderable::with([
                header,
                Box::new(title),
                Box::new(subtitle),
            ]));
        }
        let mut model = HelixState::new(HelixConfig {
            title: String::new(),
            placeholder: String::new(),
            empty_text: "no matches".to_owned(),
            no_match_text: "no matches".to_owned(),
            filter_keys: &[],
            preview: false,
            multi: false,
            searchable: params.is_searchable,
            on_demand: true,
            numbered: true,
            subtitle: None,
        });
        let mut rows = Vec::with_capacity(params.items.len());
        let mut actions = Vec::with_capacity(params.items.len());
        let mut section: Option<String> = None;
        let mut current = None;
        for (index, item) in params.items.into_iter().enumerate() {
            if item.is_header {
                // A header names the section of the rows after it; it is not a row.
                section = Some(item.name);
                actions.push(None);
                continue;
            }
            if item.is_current && current.is_none() {
                current = Some(index);
            }
            actions.push(Some((item.actions, item.dismiss_on_select)));
            rows.push(Row {
                index,
                name: item.name,
                description: item.description,
                selected_description: item.selected_description,
                is_current: item.is_current,
                display_shortcut: item.display_shortcut,
                search_value: item.search_value,
                section: section.clone(),
            });
        }
        model.set_items(rows);
        model.refresh();
        // The row that starts selected: the one asked for, else (in a list with no search) the
        // current one, else the first.
        let start = params
            .initial_selected_idx
            .or_else(|| (!params.is_searchable).then_some(current).flatten());
        if let Some(index) = start {
            model.select(index);
        }
        let mut view = Self {
            footer_hint: params.footer_hint,
            footer_hint_right: params.footer_hint_right,
            model,
            actions,
            scroll: ScrollState::new(),
            complete: false,
            app_event_tx,
            is_searchable: params.is_searchable,
            search_placeholder: params.is_searchable.then(|| {
                params
                    .search_placeholder
                    .unwrap_or_else(|| "Type to filter".to_string())
            }),
            last_selected_actual_idx: None,
            header,
            on_dismiss: params.on_dismiss,
            on_shift_tab: params.on_shift_tab,
        };
        view.sync_scroll();
        view
    }

    pub(crate) fn update_item(
        &mut self,
        stable_id: &str,
        name: String,
        description: Option<String>,
        search_value: String,
    ) -> bool {
        let Some(index) = self.index_of(stable_id) else {
            return false;
        };
        self.model.update(index, |row| {
            row.name = name;
            row.description = description;
            row.search_value = Some(search_value);
        });
        self.sync_scroll();
        true
    }

    pub(crate) fn remove_item(&mut self, stable_id: &str) -> bool {
        let Some(index) = self.index_of(stable_id) else {
            return false;
        };
        self.model.remove(index);
        self.sync_scroll();
        true
    }

    fn index_of(&self, stable_id: &str) -> Option<usize> {
        self.model
            .items()
            .iter()
            .find(|row| {
                row.search_value
                    .as_deref()
                    .map(search_value_id)
                    .is_some_and(|id| id == stable_id)
            })
            .map(|row| row.index)
    }

    fn max_visible_rows(len: usize) -> usize {
        MAX_POPUP_ROWS.min(len.max(1))
    }

    /// The rows as painted, and the position of the selected one among them.
    fn build_rows(&self) -> (Vec<GenericDisplayRow>, Option<usize>) {
        let show_numbers = !self.is_searchable || !self.model.searching();
        let mut rows = Vec::new();
        let mut selected = None;
        for visible in self.model.visible() {
            if let Some(section) = visible.opens_section {
                rows.push(GenericDisplayRow {
                    name: section.to_owned(),
                    display_shortcut: None,
                    match_indices: None,
                    description: None,
                    styled_description: None,
                    disabled: false,
                    is_header: true,
                    two_line: false,
                });
            }
            let item = visible.item;
            if visible.selected {
                selected = Some(rows.len());
            }
            let prefix = if visible.selected { '›' } else { ' ' };
            let name = if item.is_current {
                format!("{} (current)", item.name)
            } else {
                item.name.clone()
            };
            let display_name = match visible.number {
                Some(number) if show_numbers => format!("{prefix} {number}. {name}"),
                _ => format!("{prefix} {name}"),
            };
            let description = visible
                .selected
                .then(|| item.selected_description.clone())
                .flatten()
                .or_else(|| item.description.clone());
            rows.push(GenericDisplayRow {
                name: display_name,
                display_shortcut: item.display_shortcut,
                match_indices: None,
                description,
                styled_description: None,
                disabled: false,
                is_header: false,
                two_line: false,
            });
        }
        (rows, selected)
    }

    /// Keep the scroll window on the selected row after anything changed.
    fn sync_scroll(&mut self) {
        self.model.refresh();
        let (rows, selected) = self.build_rows();
        self.scroll.selected_idx = selected;
        self.scroll.clamp_selection(rows.len());
        self.scroll
            .ensure_visible(rows.len(), Self::max_visible_rows(rows.len()));
    }

    fn accept(&mut self) {
        let Some(index) = self.model.selected().map(HelixItem::id) else {
            self.complete = true;
            return;
        };
        self.last_selected_actual_idx = Some(index);
        if let Some(Some((actions, dismiss_on_select))) = self.actions.get(index) {
            for action in actions {
                action(&self.app_event_tx);
            }
            if *dismiss_on_select {
                self.complete = true;
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn set_search_query(&mut self, query: String) {
        self.model.set_query(query);
        self.sync_scroll();
    }

    pub(crate) fn take_last_selected_index(&mut self) -> Option<usize> {
        self.last_selected_actual_idx.take()
    }

    /// Whether the search input row should be shown.
    fn show_search_row(&self) -> bool {
        self.is_searchable && self.model.searching()
    }

    /// Compute the effective footer hint based on current state.
    fn effective_footer_hint(&self) -> Option<Line<'static>> {
        if !self.is_searchable {
            return self.footer_hint.clone();
        }
        if self.model.searching() {
            Some(Line::from(vec![
                "type to filter, ↑/↓ navigate, ".into(),
                key_hint::plain(KeyCode::Enter).into(),
                " confirm, ".into(),
                key_hint::plain(KeyCode::Esc).into(),
                " stop search".into(),
            ]))
        } else {
            Some(Line::from(vec![
                "↑/k ↓/j navigate, ".into(),
                "/ search, ".into(),
                key_hint::plain(KeyCode::Enter).into(),
                " confirm, ".into(),
                key_hint::plain(KeyCode::Esc).into(),
                " go back".into(),
            ]))
        }
    }

    fn rows_width(total_width: u16) -> u16 {
        total_width.saturating_sub(2)
    }
}

fn search_value_id(search_value: &str) -> &str {
    search_value
        .split_once(' ')
        .map_or(search_value, |(id, _)| id)
}

impl BottomPaneView for ListSelectionView {
    fn handle_key_event(&mut self, key_event: KeyEvent) {
        match self.model.on_key(key_event) {
            HelixEvent::Submit(_) => self.accept(),
            HelixEvent::Cancel | HelixEvent::Interrupt => {
                self.on_ctrl_c();
            }
            HelixEvent::Secondary => {
                if let Some(action) = &self.on_shift_tab {
                    action(&self.app_event_tx);
                }
            }
            HelixEvent::Idle
            | HelixEvent::Redraw
            | HelixEvent::Action(..)
            | HelixEvent::SubmitMany(_)
            | HelixEvent::Composed(..) => {}
        }
        self.sync_scroll();
    }

    fn is_complete(&self) -> bool {
        self.complete
    }

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        if let Some(cb) = self.on_dismiss.take() {
            cb(&self.app_event_tx);
        }
        self.complete = true;
        CancellationEvent::Handled
    }

    fn on_escape(&mut self) -> CancellationEvent {
        if self.model.searching() {
            self.model.set_query(String::new());
            self.model.stop_searching();
            self.sync_scroll();
        } else {
            self.on_ctrl_c();
        }
        CancellationEvent::Handled
    }

    fn update_selection_item(
        &mut self,
        stable_id: &str,
        name: String,
        description: Option<String>,
        search_value: String,
    ) -> bool {
        self.update_item(stable_id, name, description, search_value)
    }

    fn remove_selection_item(&mut self, stable_id: &str) -> bool {
        self.remove_item(stable_id)
    }
}

impl Renderable for ListSelectionView {
    fn desired_height(&self, width: u16) -> u16 {
        // Measure wrapped height for up to MAX_POPUP_ROWS items at the given width.
        // Build the same display rows used by the renderer so wrapping math matches.
        let (rows, _) = self.build_rows();
        let rows_width = Self::rows_width(width);
        let rows_height = measure_rows_height(&rows, &self.scroll, MAX_POPUP_ROWS, rows_width);

        // Subtract 4 for the padding on the left and right of the header.
        let mut height = self.header.desired_height(width.saturating_sub(4));
        height = height.saturating_add(rows_height + 3);
        if self.show_search_row() {
            height = height.saturating_add(1);
        }
        if self.effective_footer_hint().is_some() {
            height = height.saturating_add(1);
        }
        height
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }

        let effective_hint = self.effective_footer_hint();
        let [content_area, footer_area] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(if effective_hint.is_some() { 1 } else { 0 }),
        ])
        .areas(area);

        Block::default()
            .style(user_message_style())
            .render(content_area, buf);

        let header_height = self
            .header
            // Subtract 4 for the padding on the left and right of the header.
            .desired_height(content_area.width.saturating_sub(4));
        let (rows, _) = self.build_rows();
        let rows_width = Self::rows_width(content_area.width);
        let rows_height = measure_rows_height(&rows, &self.scroll, MAX_POPUP_ROWS, rows_width);
        let [header_area, _, search_area, list_area] = Layout::vertical([
            Constraint::Max(header_height),
            Constraint::Max(1),
            Constraint::Length(if self.show_search_row() { 1 } else { 0 }),
            Constraint::Length(rows_height),
        ])
        .areas(content_area.inset(Insets::vh(1, 2)));

        if header_area.height < header_height {
            let [header_area, elision_area] =
                Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(header_area);
            self.header.render(header_area, buf);
            Paragraph::new(vec![
                Line::from(format!("[… {header_height} lines] ctrl + a view all")).dim(),
            ])
            .render(elision_area, buf);
        } else {
            self.header.render(header_area, buf);
        }

        if self.show_search_row() {
            let query = self.model.query().to_owned();
            Line::from(query.clone()).render(search_area, buf);
            let query_span: Span<'static> = if query.is_empty() {
                self.search_placeholder
                    .as_ref()
                    .map(|placeholder| placeholder.clone().dim())
                    .unwrap_or_else(|| "".into())
            } else {
                query.into()
            };
            Line::from(query_span).render(search_area, buf);
        }

        if list_area.height > 0 {
            let render_area = Rect {
                x: list_area.x.saturating_sub(2),
                y: list_area.y,
                width: rows_width.max(1),
                height: list_area.height,
            };
            render_rows(
                render_area,
                buf,
                &rows,
                &self.scroll,
                render_area.height as usize,
                "no matches",
            );
        }

        if let Some(hint) = effective_hint {
            let hint_area = Rect {
                x: footer_area.x + 2,
                y: footer_area.y,
                width: footer_area.width.saturating_sub(2),
                height: footer_area.height,
            };
            let left_width = hint.width() as u16;
            let right_width = self
                .footer_hint_right
                .as_ref()
                .map(|line| line.width() as u16)
                .unwrap_or(0);
            hint.dim().render(hint_area, buf);
            if let Some(right_hint) = self.footer_hint_right.clone()
                && left_width.saturating_add(1).saturating_add(right_width) <= hint_area.width
            {
                let right_area = Rect {
                    x: hint_area
                        .x
                        .saturating_add(hint_area.width.saturating_sub(right_width)),
                    y: hint_area.y,
                    width: right_width,
                    height: hint_area.height,
                };
                right_hint.render(right_area, buf);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_event::AppEvent;
    use crossterm::event::KeyModifiers;
    use tokio::sync::mpsc::unbounded_channel;

    fn view(
        items: Vec<SelectionItem>,
        searchable: bool,
    ) -> (
        ListSelectionView,
        tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
    ) {
        let (tx_raw, rx) = unbounded_channel::<AppEvent>();
        let view = ListSelectionView::new(
            SelectionViewParams {
                items,
                is_searchable: searchable,
                ..Default::default()
            },
            AppEventSender::new(tx_raw),
        );
        (view, rx)
    }

    fn item(name: &str, dismiss: bool) -> SelectionItem {
        SelectionItem {
            name: name.to_owned(),
            dismiss_on_select: dismiss,
            search_value: Some(name.to_owned()),
            ..Default::default()
        }
    }

    fn header(name: &str) -> SelectionItem {
        SelectionItem {
            name: name.to_owned(),
            is_header: true,
            ..Default::default()
        }
    }

    fn press(view: &mut ListSelectionView, code: KeyCode) {
        view.handle_key_event(KeyEvent::new(code, KeyModifiers::NONE));
    }

    #[test]
    fn shift_tab_runs_action_without_dismissing() {
        let (tx_raw, mut rx) = unbounded_channel::<AppEvent>();
        let mut view = ListSelectionView::new(
            SelectionViewParams {
                items: vec![item("ElizACP", true)],
                on_shift_tab: Some(Box::new(|tx| {
                    tx.send(AppEvent::SetConfigAcpWireRecording(true));
                })),
                ..Default::default()
            },
            AppEventSender::new(tx_raw),
        );
        view.handle_key_event(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        assert!(matches!(
            rx.try_recv(),
            Ok(AppEvent::SetConfigAcpWireRecording(true))
        ));
        assert!(!view.is_complete());
    }

    #[test]
    fn digits_choose_numbered_rows_skipping_headers() {
        let (mut view, _rx) = view(
            vec![
                header("A"),
                item("one", true),
                header("B"),
                item("two", true),
            ],
            false,
        );
        press(&mut view, KeyCode::Char('2'));
        assert_eq!(view.take_last_selected_index(), Some(3));
        assert!(view.is_complete());
    }

    #[test]
    fn navigation_skips_headers_and_wraps() {
        let (mut view, _rx) = view(
            vec![
                header("A"),
                item("one", true),
                header("B"),
                item("two", true),
            ],
            false,
        );
        press(&mut view, KeyCode::Down);
        press(&mut view, KeyCode::Enter);
        assert_eq!(view.take_last_selected_index(), Some(3));
    }

    #[test]
    fn slash_opens_search_and_esc_closes_it_before_dismissing() {
        let (mut view, _rx) = view(vec![item("alpha", true), item("beta", true)], true);
        press(&mut view, KeyCode::Char('/'));
        press(&mut view, KeyCode::Char('b'));
        assert!(view.model.searching());
        assert_eq!(view.model.match_count(), 1);
        press(&mut view, KeyCode::Esc);
        assert!(!view.model.searching() && !view.is_complete());
        press(&mut view, KeyCode::Esc);
        assert!(view.is_complete());
    }

    #[test]
    fn on_dismiss_fires_on_ctrl_c_not_on_accept() {
        let (tx_raw, mut rx) = unbounded_channel::<AppEvent>();
        let mut view = ListSelectionView::new(
            SelectionViewParams {
                items: vec![item("a", true)],
                on_dismiss: Some(Box::new(|tx| {
                    tx.send(AppEvent::SetConfigAcpWireRecording(false));
                })),
                ..Default::default()
            },
            AppEventSender::new(tx_raw),
        );
        view.on_ctrl_c();
        assert!(matches!(
            rx.try_recv(),
            Ok(AppEvent::SetConfigAcpWireRecording(false))
        ));
    }
}
