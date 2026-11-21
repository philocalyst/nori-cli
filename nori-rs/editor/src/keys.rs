//! Keys: through the keymap's machine into actions, and actions onto the buffer.

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use modalkit::actions::Action;
use modalkit::actions::EditAction;
use modalkit::actions::Editable;
use modalkit::actions::EditorAction;
use modalkit::actions::HistoryAction;
use modalkit::actions::InsertTextAction;
use modalkit::actions::Jumpable;
use modalkit::actions::PromptAction;
use modalkit::actions::Searchable;
use modalkit::editing::context::EditContext;
use modalkit::editing::context::Resolve;
use modalkit::editing::cursor::CursorGroup;
use modalkit::editing::cursor::CursorState;
use modalkit::env::helix::ClipboardCommand;
use modalkit::env::helix::HelixCommand;
use modalkit::keybindings::BindingMachine;
use modalkit::prelude::CommandType;
use modalkit::prelude::MoveDir2D;
use modalkit::prelude::ScrollSize;
use modalkit::prelude::ScrollStyle;

use crate::editor::Change;
use crate::editor::Editor;
use crate::editor::EditorEvent;
use crate::info::EditorCommand;
use crate::keymap::KeymapKind;

/// What the last edit was, for grouping modeless typing into undo steps.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Typing {
    None,
    Word,
    Space,
    Deleting,
}

/// The mode the editor is in, as the host shows it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EditorMode {
    Insert,
    Normal,
    /// Helix's select mode, Vim's visual and select modes.
    Select,
    /// Vim's replace mode.
    Replace,
    /// A key menu waits for its next key (Helix `g`, `m`, `z`, space).
    Menu(Menu),
    /// The search or select prompt is open.
    Prompt,
}

/// A key menu of the Helix keymap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Menu {
    Goto,
    Match,
    View,
    Space,
}

impl EditorMode {
    /// The mode's name in full, for a footer: `INSERT`, `NORMAL`.
    pub fn name(self) -> &'static str {
        match self {
            EditorMode::Insert => "INSERT",
            EditorMode::Normal => "NORMAL",
            EditorMode::Select => "SELECT",
            EditorMode::Replace => "REPLACE",
            EditorMode::Menu(Menu::Goto) => "GOTO",
            EditorMode::Menu(Menu::Match) => "MATCH",
            EditorMode::Menu(Menu::View) => "VIEW",
            EditorMode::Menu(Menu::Space) => "SPACE",
            EditorMode::Prompt => "SEARCH",
        }
    }

    /// A short label for the mode, for a status line.
    pub fn label(self) -> &'static str {
        match self {
            EditorMode::Insert => "INS",
            EditorMode::Normal => "NOR",
            EditorMode::Select => "SEL",
            EditorMode::Replace => "REP",
            EditorMode::Menu(Menu::Goto) => "GOTO",
            EditorMode::Menu(Menu::Match) => "MATCH",
            EditorMode::Menu(Menu::View) => "VIEW",
            EditorMode::Menu(Menu::Space) => "SPACE",
            EditorMode::Prompt => "SEARCH",
        }
    }
}

/// How the terminal cursor should look.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CursorShape {
    Bar,
    Block,
    Underline,
}

impl Editor {
    /// The mode the keymap is in.
    pub fn mode(&self) -> EditorMode {
        if self.prompt.is_some() {
            return EditorMode::Prompt;
        }
        let shown = self.keys.show_mode().unwrap_or_default();
        let first = shown.split_whitespace().next().unwrap_or("");
        match self.keymap {
            KeymapKind::Helix | KeymapKind::Standard => match first {
                "insert" => EditorMode::Insert,
                "select" => EditorMode::Select,
                "goto" => EditorMode::Menu(Menu::Goto),
                "match" => EditorMode::Menu(Menu::Match),
                "view" => EditorMode::Menu(Menu::View),
                "space" => EditorMode::Menu(Menu::Space),
                "prompt" => EditorMode::Prompt,
                _ => EditorMode::Normal,
            },
            KeymapKind::Vim => {
                if shown.contains("INSERT") {
                    EditorMode::Insert
                } else if shown.contains("REPLACE") {
                    EditorMode::Replace
                } else if shown.contains("VISUAL") || shown.contains("SELECT") {
                    EditorMode::Select
                } else {
                    EditorMode::Normal
                }
            }
        }
    }

    /// What is pending in the keymap (a count, a register), for a status line.
    pub fn pending(&self) -> Option<String> {
        let shown = self.keys.show_mode()?;
        let pending: Vec<&str> = shown
            .split_whitespace()
            .filter(|word| word.contains('='))
            .collect();
        (!pending.is_empty()).then(|| pending.join(" "))
    }

    pub fn cursor_shape(&self) -> CursorShape {
        match self.mode() {
            EditorMode::Insert | EditorMode::Prompt => CursorShape::Bar,
            EditorMode::Replace => CursorShape::Underline,
            EditorMode::Normal | EditorMode::Select | EditorMode::Menu(_) => CursorShape::Block,
        }
    }

    /// Whether `Esc` means something to the editor now (leaving Insert or Select mode, closing a
    /// menu or the prompt). When it does not, the host may give `Esc` its own meaning.
    pub fn wants_escape(&self) -> bool {
        match (self.keymap, self.mode()) {
            (KeymapKind::Standard, mode) => mode == EditorMode::Prompt,
            (KeymapKind::Helix | KeymapKind::Vim, EditorMode::Normal) => self.pending().is_some(),
            (
                KeymapKind::Helix | KeymapKind::Vim,
                EditorMode::Insert
                | EditorMode::Select
                | EditorMode::Replace
                | EditorMode::Menu(_)
                | EditorMode::Prompt,
            ) => true,
        }
    }

    /// Back to Insert mode with nothing pending, as after the text was sent.
    pub fn enter_insert(&mut self) {
        self.prompt = None;
        self.keys.reset_mode();
        self.feed(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
        self.collapse_selections();
    }

    /// Drop whatever waits for its next key (an operator, a count, a register), as `Esc` does,
    /// without otherwise changing the mode.
    pub fn cancel_pending(&mut self) {
        if self.pending().is_some() {
            self.feed(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        }
    }

    /// Handle a key. Keys the host owns (submitting, history, popups) should be taken before.
    pub fn handle_key(&mut self, key: KeyEvent) -> Change {
        if key.kind == KeyEventKind::Release {
            return Change::default();
        }
        let before = (self.revision(), self.selections(), self.mode());
        let vertical = self.vertical_key(&key);
        match vertical {
            Some(down) => {
                self.move_vertically(down);
            }
            None => {
                self.goal_column = None;
                self.feed(key);
            }
        }
        // Insert mode types at a caret: a selection kept from Normal mode is let go of.
        if before.2 != EditorMode::Insert && self.mode() == EditorMode::Insert {
            self.collapse_selections();
        }
        let after = (self.revision(), self.selections(), self.mode());
        Change {
            text: before.0 != after.0,
            cursor: before.1 != after.1,
            mode: before.2 != after.2,
        }
    }

    /// Run `key` through the keymap and apply what it produces.
    fn feed(&mut self, key: KeyEvent) {
        // Screen-line motions and scrolls need the viewport the text was last laid out in.
        let wrap = usize::from(self.width.get().saturating_sub(1)).max(1);
        self.view.dimensions = (wrap, usize::from(u16::MAX));
        self.keys.input_key(key.into());
        while let Some((action, ctx)) = self.keys.pop() {
            self.apply(action, ctx);
        }
        self.sync_text();
    }

    /// `Some(down)` when `key` moves the primary cursor one visual row, which the editor does
    /// itself so it follows the rows as drawn.
    fn vertical_key(&self, key: &KeyEvent) -> Option<bool> {
        if self.prompt.is_some() || !self.group.members.is_empty() || self.pending().is_some() {
            return None;
        }
        let mode = self.mode();
        let down = match (key.code, key.modifiers, mode) {
            (KeyCode::Up, KeyModifiers::NONE, EditorMode::Insert | EditorMode::Normal) => false,
            (KeyCode::Down, KeyModifiers::NONE, EditorMode::Insert | EditorMode::Normal) => true,
            (KeyCode::Char('k'), KeyModifiers::NONE, EditorMode::Normal)
                if self.keymap == KeymapKind::Helix =>
            {
                false
            }
            (KeyCode::Char('j'), KeyModifiers::NONE, EditorMode::Normal)
                if self.keymap == KeymapKind::Helix =>
            {
                true
            }
            _ => return None,
        };
        Some(down)
    }

    /// Move the primary cursor one visual row up or down, keeping its column across moves.
    /// Returns whether it moved (it does not at the first or last row).
    pub fn move_vertically(&mut self, down: bool) -> bool {
        let layout = self.layout(self.width.get());
        let (row, column) = layout.position(&self.text.text, &self.chips, self.cursor());
        let goal = *self.goal_column.get_or_insert(column);
        let target = if down {
            row + 1
        } else {
            let Some(row) = row.checked_sub(1) else {
                return false;
            };
            row
        };
        if target >= layout.rows.len() {
            return false;
        }
        let at = layout.offset_at(&self.text.text, &self.chips, target, goal);
        let cursor = self.text.cursor(at);
        self.buffer.set_group(
            self.gid,
            CursorGroup::new(CursorState::Location(cursor), vec![]),
        );
        self.sync_text();
        true
    }

    pub(crate) fn apply(&mut self, action: Action<crate::info::EditorInfo>, ctx: EditContext) {
        match action {
            Action::Editor(action) => self.editor_action(action, &ctx),
            Action::Macro(action) => {
                let _ = self.keys.macro_command(&action, &ctx, &mut self.store);
            }
            Action::Repeat(repeat) => self.keys.repeat(repeat, Some(ctx)),
            Action::Search(dir, count) => {
                if let Err(error) =
                    self.buffer
                        .search(dir, count, &(self.gid, &self.view, &ctx), &mut self.store)
                {
                    self.events.push(EditorEvent::Notice(error.to_string()));
                }
            }
            Action::Jump(list, dir, count) => {
                let count = ctx.resolve(&count);
                let _ = self
                    .buffer
                    .jump(list, dir, count, &(self.gid, &self.view, &ctx));
            }
            Action::Scroll(style) => self.scroll(&style),
            Action::CommandBar(action) => self.command_bar(action),
            Action::Prompt(PromptAction::Submit) => self.submit_prompt(&ctx),
            Action::Prompt(PromptAction::Abort(_)) => self.prompt = None,
            Action::Prompt(PromptAction::Recall(..)) => {}
            Action::Application(command) => self.command(command, &ctx),
            Action::NoOp
            | Action::KeywordLookup(_)
            | Action::RedrawScreen
            | Action::ShowInfoMessage(_)
            | Action::Suspend
            | Action::Command(_)
            | Action::Tab(_)
            | Action::Window(_) => {}
            // `Action` is non-exhaustive: what a newer modalkit adds is ignored until handled.
            _ => {}
        }
    }

    fn editor_action(&mut self, action: EditorAction, ctx: &EditContext) {
        if let Some(prompt) = self.prompt.as_mut() {
            prompt.editor_action(&action, ctx, &mut self.store);
            return;
        }
        let typing = match &action {
            EditorAction::InsertText(InsertTextAction::Type(ch, ..)) => {
                let blank = ctx
                    .resolve(ch)
                    .and_then(|ch| match ch {
                        modalkit::prelude::Char::Single(ch) => Some(ch.is_whitespace()),
                        _ => None,
                    })
                    .unwrap_or(false);
                if blank { Typing::Space } else { Typing::Word }
            }
            EditorAction::Edit(spec, _) if matches!(ctx.resolve(spec), EditAction::Delete(..)) => {
                Typing::Deleting
            }
            EditorAction::History(HistoryAction::Undo(_) | HistoryAction::Redo(_)) => {
                // Whatever was typed since the last step is a step of its own first.
                self.checkpoint();
                Typing::None
            }
            _ => Typing::None,
        };
        if self.keymap == KeymapKind::Standard && starts_undo_step(self.typing, typing) {
            self.checkpoint();
        }
        // Modeless: typing or deleting over a selection replaces it, as in any text field.
        if self.keymap == KeymapKind::Standard
            && matches!(typing, Typing::Word | Typing::Space | Typing::Deleting)
            && let Some(range) = self
                .selections()
                .iter()
                .find(|selection| selection.selected && selection.anchor != selection.head)
                .map(|selection| selection.range(&self.text.text))
        {
            self.replace_range(range, "");
            if typing == Typing::Deleting {
                return;
            }
        }
        self.typing = typing;
        if let Err(error) =
            self.buffer
                .editor_command(&action, &(self.gid, &self.view, ctx), &mut self.store)
        {
            self.events.push(EditorEvent::Notice(error.to_string()));
        }
    }

    fn scroll(&mut self, style: &ScrollStyle) {
        let rows = match style {
            ScrollStyle::Direction2D(MoveDir2D::Up | MoveDir2D::Down, ScrollSize::Cell, _) => 1,
            ScrollStyle::Direction2D(MoveDir2D::Up | MoveDir2D::Down, ScrollSize::HalfPage, _) => 5,
            ScrollStyle::Direction2D(MoveDir2D::Up | MoveDir2D::Down, ScrollSize::Page, _) => 10,
            _ => return,
        };
        let down = matches!(style, ScrollStyle::Direction2D(MoveDir2D::Down, ..));
        for _ in 0..rows {
            if !self.move_vertically(down) {
                break;
            }
        }
    }

    fn command(&mut self, command: EditorCommand, ctx: &EditContext) {
        match command {
            EditorCommand::Helix(HelixCommand::Word(motion, extend)) => {
                self.word_motion(motion, extend, ctx);
            }
            EditorCommand::Helix(HelixCommand::Indent(more)) => {
                let count = ctx.resolve(&modalkit::prelude::Count::Contextual);
                self.indent_lines(more, count);
            }
            EditorCommand::Helix(HelixCommand::SelectLines(below)) => {
                let count = ctx.resolve(&modalkit::prelude::Count::Contextual);
                self.select_lines(below, count);
            }
            EditorCommand::Helix(HelixCommand::SurroundAdd(ch)) => self.surround_add(ch),
            EditorCommand::Helix(HelixCommand::SurroundDelete(ch)) => self.surround_delete(ch),
            EditorCommand::Helix(HelixCommand::SurroundReplace(from, to)) => {
                self.surround_replace(from, to);
            }
            EditorCommand::Helix(HelixCommand::SearchSelection) => {
                let primary = self.primary();
                let selected = &self.text.text[primary.range(&self.text.text)];
                let pattern = regex::escape(selected);
                self.store
                    .registers
                    .set_last_command(CommandType::Search, pattern.as_str());
            }
            EditorCommand::Helix(HelixCommand::CommandPalette) => {
                self.events.push(EditorEvent::CommandPalette);
            }
            EditorCommand::Helix(HelixCommand::Lsp(request)) => {
                self.events.push(EditorEvent::Lsp(request, self.cursor()));
            }
            EditorCommand::Helix(HelixCommand::Clipboard(ClipboardCommand::Yank)) => {
                let text = self.selected_text().join("\n");
                self.events.push(EditorEvent::CopyToClipboard(text));
            }
            EditorCommand::Helix(HelixCommand::Clipboard(paste)) => {
                self.events.push(EditorEvent::PasteFromClipboard(paste));
            }
            EditorCommand::Host(id) => self.events.push(EditorEvent::Host(id)),
        }
    }

    /// Let go of every selection, keeping a cursor at each head.
    pub(crate) fn collapse_selections(&mut self) {
        let states = self
            .group
            .iter()
            .map(|state| CursorState::Location(state.cursor().clone()))
            .collect();
        self.set_states(states);
    }

    /// The text of every selection (a bare cursor's character), primary first.
    pub fn selected_text(&self) -> Vec<String> {
        self.selections()
            .iter()
            .map(|selection| self.text.text[selection.range(&self.text.text)].to_owned())
            .collect()
    }

    /// Answer a [EditorEvent::PasteFromClipboard] with the clipboard's text.
    pub fn paste_clipboard(&mut self, command: ClipboardCommand, text: &str) {
        match command {
            ClipboardCommand::Yank => {}
            ClipboardCommand::PasteBefore => {
                let starts: Vec<usize> = self
                    .selections()
                    .iter()
                    .map(|selection| selection.range(&self.text.text).start)
                    .collect();
                self.insert_at(&starts, text);
            }
            ClipboardCommand::PasteAfter => {
                let ends: Vec<usize> = self
                    .selections()
                    .iter()
                    .map(|selection| selection.range(&self.text.text).end)
                    .collect();
                self.insert_at(&ends, text);
            }
            ClipboardCommand::Replace => {
                let mut ranges: Vec<_> = self
                    .selections()
                    .iter()
                    .map(|selection| selection.range(&self.text.text))
                    .collect();
                ranges.sort_by_key(|range| range.start);
                let mut new = String::new();
                let mut copied = 0;
                for range in ranges {
                    if range.start < copied {
                        continue;
                    }
                    new.push_str(&self.text.text[copied..range.start]);
                    new.push_str(text);
                    copied = range.end;
                }
                new.push_str(&self.text.text[copied..]);
                let cursor = self.cursor();
                self.replace_text(&new, cursor);
            }
        }
    }

    fn insert_at(&mut self, offsets: &[usize], text: &str) {
        let mut edits: Vec<(usize, &str)> = offsets.iter().map(|at| (*at, text)).collect();
        edits.sort_by_key(|(at, _)| *at);
        edits.dedup_by_key(|(at, _)| *at);
        self.checkpoint();
        self.apply_insertions(&edits);
        self.checkpoint();
    }
}

/// Whether going from typing `previous` to `next` starts a new undo step: at each word, and
/// between typing and deleting.
fn starts_undo_step(previous: Typing, next: Typing) -> bool {
    match (previous, next) {
        (Typing::Word, Typing::Word)
        | (Typing::Space, Typing::Space)
        | (Typing::Word, Typing::Space)
        | (Typing::Deleting, Typing::Deleting)
        | (_, Typing::None) => false,
        (Typing::Space, Typing::Word)
        | (Typing::None | Typing::Deleting, Typing::Word | Typing::Space)
        | (Typing::None | Typing::Word | Typing::Space, Typing::Deleting) => true,
    }
}
