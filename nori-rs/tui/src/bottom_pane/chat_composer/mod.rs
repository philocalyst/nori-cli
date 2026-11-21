use crate::key_hint::has_ctrl_or_alt;
use crate::nori::hotkey_match::matches_binding;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use ratatui::buffer::Buffer;
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use ratatui::layout::Margin;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Block;
use ratatui::widgets::Widget;
use ratatui::widgets::WidgetRef;

use super::chat_composer_history::ChatComposerHistory;
use super::command_popup::CommandItem;
use super::command_popup::CommandPopup;
use super::file_search_popup::FileSearchPopup;
use super::footer::FooterMode;
use super::footer::FooterProps;
use super::footer::footer_height;
use super::footer::render_footer;
use super::footer::reset_mode_after_activity;
use super::footer::textarea_corner_segments;
use super::history_search_popup::HistorySearchPopup;
use super::paste_burst::CharDecision;
use super::paste_burst::PasteBurst;
use super::skill_popup::SkillPickerItem;
use super::skill_popup::SkillPopup;
use crate::bottom_pane::paste_burst::FlushResult;
use crate::bottom_pane::prompt_args::expand_custom_prompt;
use crate::bottom_pane::prompt_args::expand_if_numeric_with_positional_args;
use crate::bottom_pane::prompt_args::extract_positional_args_for_prompt_line;
use crate::bottom_pane::prompt_args::parse_slash_name;
use crate::bottom_pane::prompt_args::prompt_argument_names;
use crate::bottom_pane::prompt_args::prompt_command_with_arg_placeholders;
use crate::bottom_pane::prompt_args::prompt_has_numeric_placeholders;
use crate::render::Insets;
use crate::render::RectExt;
use crate::render::renderable::Renderable;
use crate::slash_command::SlashCommand;
use crate::slash_command::built_in_slash_commands;
use crate::style::user_message_style;
use nori_harness::custom_prompts::CustomPrompt;
use nori_harness::custom_prompts::CustomPromptKind;
use nori_harness::custom_prompts::PROMPTS_CMD_PREFIX;

use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::bottom_pane::textarea::TextArea;
use crate::clipboard_paste::normalize_pasted_path;
use crate::clipboard_paste::pasted_image_format;
use crate::history_cell;
use crate::ui_consts::LIVE_PREFIX_COLS;
use codex_file_search::FileMatch;
use nori_editor::EditorMode;
use nori_editor::KeymapKind;
use nori_editor::chips::ChipId;
use nori_editor::chips::ChipTone;
use std::cell::Cell;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

/// If the pasted content exceeds this number of characters, replace it with a
/// placeholder in the UI.
const LARGE_PASTE_CHAR_THRESHOLD: usize = 1000;
/// Result returned when the user interacts with the text area.
#[derive(Debug, PartialEq)]
pub enum InputResult {
    Submitted(String),
    /// Submit ahead of the queue by interrupting the current turn.
    Steer(String),
    /// Start the oldest queued prompt by interrupting the current turn.
    PromoteQueued,
    Command(SlashCommand),
    /// An action chosen in the palette or typed as a command.
    Action(nori_actions::Invocation),
    None,
}

#[derive(Clone, Debug, PartialEq)]
struct AttachedImage {
    /// The chip that stands for the image in the text.
    chip: ChipId,
    path: PathBuf,
}

enum PromptSelectionMode {
    Completion,
    Submit,
}

enum PromptSelectionAction {
    Insert { text: String, cursor: Option<usize> },
    Submit { text: String },
}

#[derive(Clone, Copy)]
enum PromptIndicator {
    Normal,
    Shortcut,
    Slash,
    Shell,
}

impl PromptIndicator {
    fn hides_textarea_prefix(self) -> bool {
        matches!(self, PromptIndicator::Slash)
    }
}

pub(crate) struct ChatComposer {
    /// Most recently rendered editable body, used for pointer hit testing.
    rendered_body_area: Cell<Option<Rect>>,
    textarea: TextArea,
    input_enabled: bool,
    active_popup: ActivePopup,
    app_event_tx: AppEventSender,
    history: ChatComposerHistory,
    ctrl_c_quit_hint: bool,
    use_shift_enter_hint: bool,
    dismissed_file_popup_token: Option<String>,
    dismissed_skill_popup_token: Option<String>,
    dismissed_command_popup_text: Option<String>,
    current_file_query: Option<String>,
    /// Large pastes, each a chip in the text: what it stands for.
    pending_pastes: Vec<(ChipId, String)>,
    is_shell_mode: bool,
    status_note: Option<crate::bottom_pane::status_line::StatusNote>,
    /// Every key the TUI answers, for the `?` list.
    keymap: nori_config::Keymap,
    has_focus: bool,
    attached_images: Vec<AttachedImage>,
    placeholder_text: String,
    is_task_running: bool,
    // Non-bracketed paste burst tracker.
    paste_burst: PasteBurst,
    // When true, disables paste-burst logic and inserts characters immediately.
    disable_paste_burst: bool,
    custom_prompts: Vec<CustomPrompt>,
    agent_commands: Vec<crate::presentation::AgentCommandInfo>,
    agent_command_prefix: String,
    command_description_overrides: HashMap<SlashCommand, Line<'static>>,
    disabled_builtin_commands: HashMap<SlashCommand, Line<'static>>,
    footer_mode: FooterMode,
    footer_hint_override: Option<Vec<(String, String)>>,
    context_window_percent: Option<i64>,
    child_context: bool,
    session_usage: Option<crate::presentation::session_runtime::SessionUsageState>,
    system_info: Option<crate::system_info::SystemInfo>,
    /// The approval mode label to display in the footer (e.g., "Read Only", "Agent", "Full Access").
    approval_mode_label: Option<String>,
    acp_mode_label: Option<String>,
    model_label: Option<String>,
    /// Cloud session id shown in the footer when attached to a cloud session.
    cloud_session: Option<String>,
    vim_enter_behavior: nori_config::VimEnterBehavior,
    /// `[tui] editor_mode` is Helix (or Kakoune, its old name).
    helix_keymap: bool,
    /// When Esc last took the editor out of Insert mode: an Esc soon after does not also
    /// interrupt a running turn.
    left_insert_at: Option<Instant>,
    vertical_footer: bool,
    prompt_summary: Option<String>,
    /// Agent-supplied session title, sanitized for single-line display.
    session_title: Option<String>,
    footer_segment_config: nori_config::FooterSegmentConfig,
    footer_layout_config: nori_config::FooterLayoutConfig,
    active_subagent_count: i64,
    background: super::footer::Background,
    /// The prompts of the main conversation, for walking back with `↑`.
    conversation_prompts: prompt_walk::ConversationPrompts,
    /// The walk back through them, while one is on.
    walk: Option<prompt_walk::PromptWalk>,
    /// Attachments of a draft restored after a recalled prompt was sent, held until the widget
    /// has taken the ones sent with it.
    held_images: Vec<AttachedImage>,
    /// Every action (built-in, the agent's, plugins'), for the palette and typed commands.
    actions: std::sync::Arc<nori_actions::Registry>,
    /// The dials' values the agent offers now, for the palette.
    action_choices: super::palette::ActionChoices,
    /// Directives in the draft: the chip that stands for each and what it does on send.
    directives: Vec<(ChipId, nori_actions::Invocation)>,
    /// The actions plugins define (the Pkl runtime's), merged into `actions`.
    plugin_actions: std::sync::Arc<[nori_actions::Action]>,
    /// The draft `Ctrl+C` cleared last, which `↓` on an empty composer brings back.
    cleared_draft: Option<String>,
    /// The directives of the prompt just sent, until the widget takes them.
    sent_directives: Vec<(ChipId, nori_actions::Invocation)>,
}

/// Popup state – at most one can be visible at any time.
enum ActivePopup {
    None,
    Command(CommandPopup),
    Skill(SkillPopup),
    File(FileSearchPopup),
    HistorySearch(HistorySearchPopup),
    Palette(Box<super::palette::Palette>),
}

const FOOTER_SPACING_HEIGHT: u16 = 0;
/// One padding row above the textarea, the textarea row, one padding row
/// below. The padding rows are where the textarea corner segments render.
const MIN_COMPOSER_HEIGHT: u16 = 3;

mod draft;
mod editor_events;
mod palette_handling;
pub(crate) use draft::ComposerDraft;
mod inline_walk;
mod key_handling;
mod paste_handling;
mod popup_management;
mod prompt_walk;
pub(crate) use prompt_walk::PromptsFingerprint;
pub(crate) use prompt_walk::RecalledPrompt;
pub(crate) use prompt_walk::WalkBadge;
mod rendering;

impl ChatComposer {
    pub fn new(
        has_input_focus: bool,
        app_event_tx: AppEventSender,
        enhanced_keys_supported: bool,
        placeholder_text: String,
        disable_paste_burst: bool,
    ) -> Self {
        let use_shift_enter_hint = enhanced_keys_supported;

        let mut this = Self {
            rendered_body_area: Cell::new(None),
            textarea: TextArea::new(),
            input_enabled: true,
            active_popup: ActivePopup::None,
            app_event_tx,
            history: ChatComposerHistory::new(),
            ctrl_c_quit_hint: false,
            use_shift_enter_hint,
            dismissed_file_popup_token: None,
            dismissed_skill_popup_token: None,
            dismissed_command_popup_text: None,
            current_file_query: None,
            pending_pastes: Vec::new(),
            is_shell_mode: false,
            status_note: None,
            keymap: nori_config::Keymap::default(),
            has_focus: has_input_focus,
            attached_images: Vec::new(),
            placeholder_text,
            is_task_running: false,
            paste_burst: PasteBurst::default(),
            disable_paste_burst: false,
            custom_prompts: Vec::new(),
            agent_commands: Vec::new(),
            agent_command_prefix: String::new(),
            command_description_overrides: HashMap::new(),
            disabled_builtin_commands: HashMap::new(),
            footer_mode: FooterMode::ShortcutSummary,
            footer_hint_override: None,
            context_window_percent: None,
            child_context: false,
            session_usage: None,
            system_info: None,
            approval_mode_label: None,
            acp_mode_label: None,
            model_label: None,
            cloud_session: None,
            vim_enter_behavior: nori_config::VimEnterBehavior::Off,
            helix_keymap: false,
            left_insert_at: None,
            vertical_footer: false,
            prompt_summary: None,
            session_title: None,
            footer_segment_config: nori_config::FooterSegmentConfig::default(),
            footer_layout_config: nori_config::FooterLayoutConfig::default(),
            active_subagent_count: 0,
            background: super::footer::Background::default(),
            conversation_prompts: prompt_walk::ConversationPrompts::default(),
            walk: None,
            held_images: Vec::new(),
            actions: std::sync::Arc::new(crate::actions::builtin_registry(
                &nori_config::HotkeyConfig::default(),
            )),
            action_choices: super::palette::ActionChoices::default(),
            directives: Vec::new(),
            sent_directives: Vec::new(),
            cleared_draft: None,
            plugin_actions: std::sync::Arc::from(Vec::new()),
        };
        // Apply configuration via the setter to keep side-effects centralized.
        this.set_disable_paste_burst(disable_paste_burst);
        this
    }

    /// Returns true if the composer currently contains no user input.
    pub(crate) fn is_empty(&self) -> bool {
        !self.is_shell_mode && self.textarea.is_empty() && self.attached_images.is_empty()
    }

    /// Record the history metadata advertised by `SessionConfiguredEvent` so
    /// that the composer can navigate cross-session history.
    pub(crate) fn set_history_metadata(&mut self, log_id: u64, entry_count: usize) {
        self.history.set_metadata(log_id, entry_count);
    }

    pub(crate) fn set_vertical_footer(&mut self, vertical_footer: bool) {
        self.vertical_footer = vertical_footer;
    }

    pub(crate) fn set_footer_segment_config(&mut self, config: nori_config::FooterSegmentConfig) {
        self.footer_segment_config = config;
    }

    pub(crate) fn set_footer_layout_config(&mut self, config: nori_config::FooterLayoutConfig) {
        self.footer_layout_config = config;
    }

    pub(crate) fn set_active_subagent_count(&mut self, count: i64) {
        self.active_subagent_count = count;
    }

    pub(crate) fn set_background(&mut self, background: super::footer::Background) {
        self.background = background;
    }

    #[cfg(test)]
    pub(super) fn set_placeholder_for_test(&mut self, placeholder: &str) {
        self.placeholder_text = placeholder.to_owned();
    }

    pub(crate) fn set_keymap(&mut self, keymap: nori_config::Keymap) {
        self.keymap = keymap;
    }

    pub(crate) fn set_hotkey_config(&mut self, config: nori_config::HotkeyConfig) {
        self.textarea.set_hotkey_config(config);
        self.rebuild_actions();
    }

    pub(crate) fn set_vim_mode(&mut self, value: nori_config::VimEnterBehavior) {
        self.vim_enter_behavior = value;
        self.apply_keymap();
    }

    /// Choose the Helix keymap (`true`) or the one `vim_mode` gives.
    pub(crate) fn set_kakoune_mode(&mut self, enabled: bool) {
        self.helix_keymap = enabled;
        self.apply_keymap();
    }

    /// Choose the editor's keymap directly (the palette's "Editor keys").
    pub(crate) fn set_editor_keymap(&mut self, keymap: KeymapKind) {
        self.helix_keymap = keymap == KeymapKind::Helix;
        if keymap == KeymapKind::Vim && !self.vim_enter_behavior.is_enabled() {
            self.vim_enter_behavior = nori_config::VimEnterBehavior::Submit;
        }
        if keymap == KeymapKind::Standard {
            self.vim_enter_behavior = nori_config::VimEnterBehavior::Off;
        }
        self.apply_keymap();
    }

    fn apply_keymap(&mut self) {
        let keymap = if self.helix_keymap {
            KeymapKind::Helix
        } else if self.vim_enter_behavior.is_enabled() {
            KeymapKind::Vim
        } else {
            KeymapKind::Standard
        };
        self.textarea.set_keymap(keymap);
    }

    pub(crate) fn cursor_style(&self) -> crossterm::cursor::SetCursorStyle {
        use crossterm::cursor::SetCursorStyle;
        use nori_editor::CursorShape;
        if !self.input_enabled || self.popup_active() {
            return SetCursorStyle::DefaultUserShape;
        }
        match (self.textarea.keymap(), self.textarea.cursor_shape()) {
            (KeymapKind::Standard, _) => SetCursorStyle::DefaultUserShape,
            (KeymapKind::Helix | KeymapKind::Vim, CursorShape::Bar) => SetCursorStyle::SteadyBar,
            (KeymapKind::Helix | KeymapKind::Vim, CursorShape::Block) => {
                SetCursorStyle::SteadyBlock
            }
            (KeymapKind::Helix | KeymapKind::Vim, CursorShape::Underline) => {
                SetCursorStyle::SteadyUnderScore
            }
        }
    }

    pub(crate) fn place_cursor_at(&mut self, column: u16, row: u16, area: Rect) -> bool {
        if !self.input_enabled || self.popup_active() {
            return false;
        }
        let Some(body) = self.rendered_body_area.get() else {
            return false;
        };
        if body.intersection(area) != body {
            return false;
        }
        self.textarea.place_cursor_at(column, row)
    }

    /// Whether the editor has a use for this Esc (leaving Insert mode, closing a menu).
    pub(crate) fn should_handle_vim_insert_escape(&self, key_event: KeyEvent) -> bool {
        key_event.code == KeyCode::Esc
            && key_event.modifiers == KeyModifiers::NONE
            && matches!(key_event.kind, KeyEventKind::Press | KeyEventKind::Repeat)
            && self.textarea.wants_escape()
    }

    /// Whether Esc during a running turn is the composer's rather than an interrupt: the
    /// editor or a walk wants it, or it follows an Esc that left Insert mode a moment ago.
    pub(crate) fn should_handle_vim_escape_during_task(&self, key_event: KeyEvent) -> bool {
        const DOUBLE_ESCAPE: Duration = Duration::from_millis(500);
        self.popup_active()
            || self.walk.is_some()
            || self.should_handle_vim_insert_escape(key_event)
            || key_event.code == KeyCode::Esc
                && (key_event.kind == KeyEventKind::Repeat
                    || self
                        .left_insert_at
                        .is_some_and(|at| at.elapsed() < DOUBLE_ESCAPE))
    }

    pub(crate) fn is_vim_operator_pending(&self) -> bool {
        self.textarea.is_pending()
    }

    pub(crate) fn is_in_vim_normal_mode(&self) -> bool {
        self.textarea.is_normal_mode()
    }

    /// Set a footer segment's enabled state.
    pub(crate) fn set_footer_segment_enabled(
        &mut self,
        segment: nori_config::FooterSegment,
        enabled: bool,
    ) {
        self.footer_segment_config.set_enabled(segment, enabled);
    }

    /// The editor's mode (for testing).
    #[cfg(test)]
    pub(crate) fn editor_mode(&self) -> EditorMode {
        self.textarea.mode()
    }

    /// Integrate an asynchronous response to an on-demand history lookup. If
    /// the entry is present and the offset matches the current cursor we
    /// immediately populate the textarea.
    pub(crate) fn on_history_entry_response(
        &mut self,
        log_id: u64,
        offset: usize,
        entry: Option<String>,
    ) -> bool {
        let Some(text) = self.history.on_entry_response(log_id, offset, entry) else {
            return false;
        };
        self.set_history_content(text);
        true
    }

    /// Deliver search history results to the popup (if still open).
    pub(crate) fn on_search_history_response(&mut self, entries: Vec<nori_harness::HistoryEntry>) {
        if let ActivePopup::HistorySearch(popup) = &mut self.active_popup {
            popup.set_entries(entries);
        }
    }

    /// Override the footer hint items displayed beneath the composer. Passing
    /// `None` restores the default shortcut footer.
    pub(crate) fn set_footer_hint_override(&mut self, items: Option<Vec<(String, String)>>) {
        self.footer_hint_override = items;
    }

    pub(super) fn input_enabled(&self) -> bool {
        self.input_enabled
    }

    /// Replace the draft with `text`, discard buffered input, and place the cursor
    /// at the end. Widget handoffs must use `take_draft` / `restore_draft`.
    pub(crate) fn set_text_content(&mut self, text: String) {
        if !self.input_enabled {
            return;
        }
        self.paste_burst = PasteBurst::default();
        // Clear any existing content, placeholders, and attachments first.
        self.textarea.set_text("");
        self.pending_pastes.clear();
        self.attached_images.clear();
        if let Some(shell_body) = text.strip_prefix('!') {
            self.is_shell_mode = true;
            self.textarea.set_text(shell_body);
        } else {
            self.is_shell_mode = false;
            self.textarea.set_text(&text);
        }
        self.textarea.set_cursor(self.textarea.text().len());
        self.sync_selection_popups();
    }

    /// Restore text rejected after submission without discarding draft attachments.
    pub(crate) fn restore_submission_text(&mut self, text: String) {
        if !self.input_enabled {
            return;
        }
        self.is_shell_mode = false;
        self.textarea.set_text(&text);
        self.textarea.set_cursor(text.len());
        self.sync_selection_popups();
    }

    fn set_history_content(&mut self, text: String) {
        if !self.input_enabled {
            return;
        }
        self.set_text_content(text);
    }

    pub(crate) fn clear_for_ctrl_c(&mut self) -> Option<String> {
        if !self.input_enabled || self.is_empty() {
            return None;
        }
        let previous = self.current_text();
        self.set_text_content(String::new());
        self.history.reset_navigation();
        // `↓` on the empty composer brings it back.
        self.cleared_draft = Some(previous.clone());
        Some(previous)
    }

    pub(crate) fn set_status_note(
        &mut self,
        note: Option<crate::bottom_pane::status_line::StatusNote>,
    ) {
        self.status_note = note;
    }

    pub(crate) fn status_note(&self) -> Option<&crate::bottom_pane::status_line::StatusNote> {
        self.status_note.as_ref()
    }

    pub(crate) fn shell_text_is(&self, text: &str) -> bool {
        self.is_shell_mode && self.textarea.text() == text
    }

    pub(crate) fn replace_shell_range(&mut self, range: std::ops::Range<usize>, text: &str) {
        if !self.is_shell_mode || range.end > self.textarea.text().len() {
            return;
        }
        let cursor = range.start + text.len();
        self.textarea.replace_range(range, text);
        self.textarea.set_cursor(cursor);
    }

    /// Get the current composer text.
    pub(crate) fn current_text(&self) -> String {
        if self.is_shell_mode {
            format!("!{}", self.textarea.text())
        } else {
            self.textarea.text().to_string()
        }
    }

    /// Attempt to start a burst by retro-capturing recent chars before the cursor.
    pub fn attach_image(&mut self, path: PathBuf, width: u32, height: u32, _format_label: &str) {
        if !self.input_enabled {
            return;
        }
        let file_label = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "image".to_string());
        let label = format!("[{file_label} {width}x{height}]");
        let chip = self.textarea.insert_chip(&label, ChipTone::Attachment);
        self.attached_images.push(AttachedImage { chip, path });
    }

    pub fn take_recent_submission_images(&mut self) -> Vec<PathBuf> {
        let images = std::mem::take(&mut self.attached_images);
        self.attached_images = std::mem::take(&mut self.held_images);
        images.into_iter().map(|img| img.path).collect()
    }

    /// Integrate results from an asynchronous file search.
    pub(crate) fn on_file_search_result(&mut self, query: String, matches: Vec<FileMatch>) {
        // Only apply if user is still editing a token starting with `query`.
        let current_opt = Self::current_at_token(&self.textarea);
        let Some(current_token) = current_opt else {
            return;
        };

        if !current_token.starts_with(&query) {
            return;
        }

        if let ActivePopup::File(popup) = &mut self.active_popup {
            popup.set_matches(&query, matches);
        }
    }

    pub fn set_ctrl_c_quit_hint(&mut self, show: bool, has_focus: bool) {
        self.ctrl_c_quit_hint = show;
        if show {
            self.footer_mode = FooterMode::CtrlCReminder;
        } else {
            self.footer_mode = reset_mode_after_activity(self.footer_mode);
        }
        self.set_has_focus(has_focus);
    }

    pub(crate) fn insert_str(&mut self, text: &str) {
        if !self.input_enabled {
            return;
        }
        self.textarea.insert_str(text);
        self.sync_selection_popups();
    }

    pub(crate) fn show_exit_in_progress(&mut self) {
        self.input_enabled = false;
        self.active_popup = ActivePopup::None;
        self.footer_hint_override = Some(Vec::new());
        self.ctrl_c_quit_hint = false;
        self.paste_burst.clear_after_explicit_paste();
    }

    fn set_has_focus(&mut self, has_focus: bool) {
        self.has_focus = has_focus;
    }

    pub fn set_task_running(&mut self, running: bool) {
        self.is_task_running = running;
    }

    pub(crate) fn set_session_usage(
        &mut self,
        usage: Option<crate::presentation::session_runtime::SessionUsageState>,
    ) {
        self.session_usage = usage;
    }

    pub(crate) fn set_child_context(&mut self, child: bool) {
        self.child_context = child;
    }

    pub(crate) fn set_system_info(&mut self, info: crate::system_info::SystemInfo) {
        self.system_info = Some(info);
    }

    pub(crate) fn set_approval_mode_label(&mut self, label: Option<String>) {
        self.approval_mode_label = label;
    }

    pub(crate) fn set_acp_mode_label(&mut self, label: Option<String>) {
        self.acp_mode_label = label;
    }

    #[cfg(test)]
    pub(crate) fn model_label(&self) -> Option<&str> {
        self.model_label.as_deref()
    }

    pub(crate) fn set_model_label(&mut self, label: Option<String>) {
        self.model_label = label;
    }

    pub(crate) fn set_cloud_session(&mut self, cloud_session: Option<String>) {
        self.cloud_session = cloud_session;
    }

    pub(crate) fn set_prompt_summary(&mut self, summary: Option<String>) {
        self.prompt_summary = summary;
    }

    pub(crate) fn set_session_title(&mut self, title: Option<String>) {
        self.session_title = title;
    }

    pub(crate) fn set_command_description_override(&mut self, cmd: SlashCommand, desc: String) {
        self.command_description_overrides
            .insert(cmd, Line::from(desc));
    }

    pub(crate) fn set_builtin_command_disabled(
        &mut self,
        cmd: SlashCommand,
        reason: Option<Line<'static>>,
    ) {
        if let Some(reason) = reason {
            self.disabled_builtin_commands.insert(cmd, reason);
        } else {
            self.disabled_builtin_commands.remove(&cmd);
        }
        if let ActivePopup::Command(popup) = &mut self.active_popup {
            popup.set_disabled_builtins(self.disabled_builtin_commands.clone());
        }
    }
}

impl ChatComposer {
    /// Rows an open popup adds to the composer's own height: what it takes beyond the footer it
    /// stands in for.
    pub(crate) fn popup_rows(&self, width: u16) -> u16 {
        let popup_height = match &self.active_popup {
            ActivePopup::None => return 0,
            ActivePopup::Command(c) => c.calculate_required_height(width),
            ActivePopup::Skill(c) => c.calculate_required_height(width),
            ActivePopup::File(c) => c.calculate_required_height(),
            ActivePopup::HistorySearch(c) => c.calculate_required_height(),
            ActivePopup::Palette(c) => c.calculate_required_height(width),
        };
        let footer_props = self.footer_props();
        let footer_hint_height = self
            .custom_footer_height()
            .unwrap_or_else(|| footer_height(&footer_props));
        popup_height.saturating_sub(footer_hint_height + Self::footer_spacing(footer_hint_height))
    }
}

impl Renderable for ChatComposer {
    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        // A prompt edited in place has its cursor in the conversation, where the stage puts it.
        if !self.input_enabled || self.walks_inline() {
            return None;
        }
        let [_, textarea_rect, popup_rect] = self.layout_areas(area);
        if let ActivePopup::Palette(palette) = &self.active_popup {
            return palette.cursor_pos(popup_rect);
        }
        let textarea_rect = Self::textarea_body_rect(textarea_rect, self.prompt_indicator());
        self.textarea.cursor_pos(textarea_rect)
    }

    fn desired_height(&self, width: u16) -> u16 {
        if !self.input_enabled {
            return 3;
        }
        if self.walks_inline() {
            return self.inline_walk_height();
        }
        let footer_props = self.footer_props();
        let footer_hint_height = self
            .custom_footer_height()
            .unwrap_or_else(|| footer_height(&footer_props));
        let footer_spacing = Self::footer_spacing(footer_hint_height);
        let footer_total_height = footer_hint_height + footer_spacing;
        const COLS_WITH_MARGIN: u16 = LIVE_PREFIX_COLS + 1;
        let hidden_prefix_cols = if self.prompt_indicator().hides_textarea_prefix() {
            1
        } else {
            0
        };
        self.textarea.desired_height(
            width
                .saturating_sub(COLS_WITH_MARGIN)
                .saturating_add(hidden_prefix_cols),
        ) + 2
            + match &self.active_popup {
                ActivePopup::None => footer_total_height,
                ActivePopup::Command(c) => c.calculate_required_height(width),
                ActivePopup::Skill(c) => c.calculate_required_height(width),
                ActivePopup::File(c) => c.calculate_required_height(),
                ActivePopup::HistorySearch(c) => c.calculate_required_height(),
                ActivePopup::Palette(c) => c.calculate_required_height(width),
            }
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        if self.input_enabled && self.walks_inline() {
            self.render_inline_walk(area, buf);
            return;
        }
        let [composer_rect, textarea_rect, popup_rect] = self.layout_areas(area);
        if !self.input_enabled {
            // Frozen exit state: no popups, no footer, no cursor — just a dim
            // prompt and message over the composer block.
            Block::default()
                .style(user_message_style())
                .render(composer_rect, buf);
            if !textarea_rect.is_empty() {
                let prompt = "›".dim();
                buf.set_span(
                    textarea_rect.x - LIVE_PREFIX_COLS,
                    textarea_rect.y,
                    &prompt,
                    textarea_rect.width,
                );
                Line::from("Exiting…".dim()).render(textarea_rect, buf);
            }
            return;
        }
        match &self.active_popup {
            ActivePopup::Command(popup) => {
                popup.render_ref(popup_rect, buf);
            }
            ActivePopup::Skill(popup) => {
                popup.render_ref(popup_rect, buf);
            }
            ActivePopup::File(popup) => {
                popup.render_ref(popup_rect, buf);
            }
            ActivePopup::HistorySearch(popup) => {
                popup.render_ref(popup_rect, buf);
            }
            ActivePopup::Palette(popup) => {
                popup.render(popup_rect, buf);
            }
            ActivePopup::None => {
                let footer_props = self.footer_props();
                let custom_height = self.custom_footer_height();
                let footer_hint_height =
                    custom_height.unwrap_or_else(|| footer_height(&footer_props));
                let footer_spacing = Self::footer_spacing(footer_hint_height);
                let hint_rect = if footer_spacing > 0 && footer_hint_height > 0 {
                    let [_, hint_rect] = Layout::vertical([
                        Constraint::Length(footer_spacing),
                        Constraint::Length(footer_hint_height),
                    ])
                    .areas(popup_rect);
                    hint_rect
                } else {
                    popup_rect
                };
                if let Some(items) = self.footer_hint_override.as_ref() {
                    if !items.is_empty() {
                        let mut spans = Vec::with_capacity(items.len() * 4);
                        for (idx, (key, label)) in items.iter().enumerate() {
                            spans.push(" ".into());
                            spans.push(Span::styled(key.clone(), Style::default().bold()));
                            spans.push(format!(" {label}").into());
                            if idx + 1 != items.len() {
                                spans.push("   ".into());
                            }
                        }
                        let mut custom_rect = hint_rect;
                        if custom_rect.width > 2 {
                            custom_rect.x += 2;
                            custom_rect.width = custom_rect.width.saturating_sub(2);
                        }
                        Line::from(spans).render(custom_rect, buf);
                    }
                } else {
                    render_footer(hint_rect, buf, &footer_props);
                }
            }
        }
        let style = user_message_style();
        Block::default().style(style).render(composer_rect, buf);
        let mut corners = textarea_corner_segments(&self.footer_props());
        if let Some(badge) = self.walk_badge() {
            corners.top_left = badge.hint(composer_rect.width.saturating_sub(4));
            corners.top_right = Line::default();
        }
        render_textarea_corner_segments(composer_rect, buf, corners);
        let prompt_indicator = self.prompt_indicator();
        if !textarea_rect.is_empty() {
            let prompt = match prompt_indicator {
                PromptIndicator::Normal => "›".bold(),
                PromptIndicator::Shortcut => {
                    Span::styled("?", Style::default().fg(Color::DarkGray))
                }
                PromptIndicator::Slash => "/".green(),
                PromptIndicator::Shell => "!".red(),
            };
            buf.set_span(
                textarea_rect.x - LIVE_PREFIX_COLS,
                textarea_rect.y,
                &prompt,
                textarea_rect.width,
            );
        }

        let body_rect = Self::textarea_body_rect(textarea_rect, prompt_indicator);
        self.rendered_body_area.set(Some(body_rect));
        self.textarea.set_shell_highlight(self.is_shell_mode);
        self.textarea.render(body_rect, buf);
        if prompt_indicator.hides_textarea_prefix() && !textarea_rect.is_empty() {
            buf.set_string(
                textarea_rect.x.saturating_sub(1),
                textarea_rect.y,
                " ",
                user_message_style(),
            );
        }
        if !self.is_shell_mode && self.textarea.text().is_empty() {
            let placeholder_text = if matches!(prompt_indicator, PromptIndicator::Shortcut) {
                self.placeholder_text
                    .strip_prefix("? ")
                    .unwrap_or(&self.placeholder_text)
            } else {
                &self.placeholder_text
            };
            let placeholder = Span::from(placeholder_text).dim();
            Line::from(vec![placeholder]).render(body_rect.inner(Margin::new(0, 0)), buf);
        }
    }
}

impl ChatComposer {
    fn textarea_body_rect(textarea_rect: Rect, prompt_indicator: PromptIndicator) -> Rect {
        if prompt_indicator.hides_textarea_prefix() {
            Rect {
                x: textarea_rect.x.saturating_sub(1),
                width: textarea_rect.width.saturating_add(1),
                ..textarea_rect
            }
        } else {
            textarea_rect
        }
    }

    fn prompt_indicator(&self) -> PromptIndicator {
        if self.is_shell_mode {
            PromptIndicator::Shell
        } else if self.textarea.text().starts_with('/') {
            PromptIndicator::Slash
        } else if matches!(self.footer_mode(), FooterMode::ShortcutOverlay) {
            PromptIndicator::Shortcut
        } else {
            PromptIndicator::Normal
        }
    }

    pub(super) fn is_editing_slash_command_name(&self) -> bool {
        let text = self.textarea.text();
        let first_line_end = text.find('\n').unwrap_or(text.len());
        let first_line = &text[..first_line_end];
        let cursor = self.textarea.cursor();
        let caret_on_first_line = cursor <= first_line_end;

        if !first_line.starts_with('/') || !caret_on_first_line {
            return false;
        }

        let token_end = first_line
            .char_indices()
            .find(|(_, c)| c.is_whitespace())
            .map(|(i, _)| i)
            .unwrap_or(first_line.len());
        cursor <= token_end
    }
}

fn render_textarea_corner_segments(
    composer_rect: Rect,
    buf: &mut Buffer,
    segments: super::footer::TextareaCornerSegments,
) {
    // Corner segments live on the composer's blank padding rows, above and
    // below the textarea. A composer squeezed below its three-row minimum has
    // no padding left, so writing a corner would land on the prompt itself.
    if composer_rect.is_empty() || composer_rect.height < MIN_COMPOSER_HEIGHT {
        return;
    }

    render_corner_row(
        composer_rect,
        composer_rect.y,
        buf,
        segments.top_left,
        segments.top_right,
    );
    render_corner_row(
        composer_rect,
        composer_rect.bottom().saturating_sub(1),
        buf,
        segments.bottom_left,
        segments.bottom_right,
    );
}

/// Keep the leading editor mode visible and clip the two groups without overlap.
fn render_corner_row(
    composer_rect: Rect,
    y: u16,
    buf: &mut Buffer,
    left: Line<'static>,
    right: Line<'static>,
) {
    let available = composer_rect.width.saturating_sub(1);
    let left_width = u16::try_from(left.width()).unwrap_or(u16::MAX);
    let right_width = u16::try_from(right.width()).unwrap_or(u16::MAX);
    let minimum_left = left_width.min(6).min(available);
    let gap = u16::from(minimum_left > 0 && right_width > 0 && minimum_left < available);
    let right_width = right_width.min(available.saturating_sub(minimum_left + gap));
    let left_width = left_width.min(available.saturating_sub(right_width + gap));
    if left_width > 0 {
        left.render(Rect::new(composer_rect.x + 1, y, left_width, 1), buf);
    }
    if right_width > 0 {
        right.render(
            Rect::new(composer_rect.right() - right_width, y, right_width, 1),
            buf,
        );
    }
}

fn prompt_selection_action(
    prompt: &CustomPrompt,
    first_line: &str,
    mode: PromptSelectionMode,
) -> PromptSelectionAction {
    let named_args = prompt_argument_names(&prompt.content);
    let has_numeric = prompt_has_numeric_placeholders(&prompt.content);

    match mode {
        PromptSelectionMode::Completion => {
            if !named_args.is_empty() {
                let (text, cursor) =
                    prompt_command_with_arg_placeholders(&prompt.name, &named_args);
                return PromptSelectionAction::Insert {
                    text,
                    cursor: Some(cursor),
                };
            }
            if has_numeric {
                let text = format!("/{PROMPTS_CMD_PREFIX}:{} ", prompt.name);
                return PromptSelectionAction::Insert { text, cursor: None };
            }
            let text = format!("/{PROMPTS_CMD_PREFIX}:{}", prompt.name);
            PromptSelectionAction::Insert { text, cursor: None }
        }
        PromptSelectionMode::Submit => {
            if !named_args.is_empty() {
                let (text, cursor) =
                    prompt_command_with_arg_placeholders(&prompt.name, &named_args);
                return PromptSelectionAction::Insert {
                    text,
                    cursor: Some(cursor),
                };
            }
            if has_numeric {
                if let Some(expanded) = expand_if_numeric_with_positional_args(prompt, first_line) {
                    return PromptSelectionAction::Submit { text: expanded };
                }
                let text = format!("/{PROMPTS_CMD_PREFIX}:{} ", prompt.name);
                return PromptSelectionAction::Insert { text, cursor: None };
            }
            PromptSelectionAction::Submit {
                text: prompt.content.clone(),
            }
        }
    }
}

#[cfg(test)]
mod tests;
