//! The editor: one modalkit buffer, its cursors and selections, a keymap, and the chips in it.

use std::cell::Cell;
use std::cell::RefCell;
use std::ops::Range;

use modalkit::actions::Action;
use modalkit::editing::buffer::CursorGroupId;
use modalkit::editing::buffer::EditBuffer;
use modalkit::editing::cursor::Cursor;
use modalkit::editing::cursor::CursorGroup;
use modalkit::editing::cursor::CursorState;
use modalkit::editing::key::KeyManager;
use modalkit::editing::rope::EditRope;
use modalkit::editing::store::Store;
use modalkit::key::TerminalKey;
use modalkit::prelude::RepeatType;
use modalkit::prelude::TargetShape;
use modalkit::prelude::ViewportContext;

use crate::chips::Chip;
use crate::chips::ChipId;
use crate::chips::Chips;
use crate::info::EditorInfo;
use crate::keymap::KeymapKind;
use crate::layout::Layout;
use crate::prompt::Prompt;
use crate::text::Text;

/// What happened to the editor that its host may need to act on.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EditorEvent {
    /// `:` (Helix): open the command palette.
    CommandPalette,
    /// A language-server request from the keymap (`gd`, `gr`, `space k`, ...), at the primary
    /// cursor (a byte offset into the text).
    Lsp(modalkit::env::helix::LspCommand, usize),
    /// `space y`: put this text on the system clipboard.
    CopyToClipboard(String),
    /// `space p` / `space P` / `space R`: the host should read the clipboard and call
    /// [Editor::paste_clipboard] with it.
    PasteFromClipboard(modalkit::env::helix::ClipboardCommand),
    /// An application command bound into the keymap, by its id.
    Host(String),
    /// Something to tell the user in passing (a search that found nothing).
    Notice(String),
}

/// One selection, as byte offsets into the text. A collapsed selection is a cursor: `anchor ==
/// head`, drawn on the character at `head`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Selection {
    pub anchor: usize,
    pub head: usize,
    /// Whether this is a selection (rather than a bare cursor).
    pub selected: bool,
}

impl Selection {
    /// The bytes it covers, end exclusive (a selection includes the character at its far end).
    pub fn range(&self, text: &str) -> Range<usize> {
        let (start, end) = if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        };
        let end = text[end.min(text.len())..]
            .chars()
            .next()
            .map_or(text.len(), |ch| end + ch.len_utf8());
        start..end
    }
}

/// What a key did.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Change {
    pub text: bool,
    pub cursor: bool,
    pub mode: bool,
}

impl Change {
    pub fn any(&self) -> bool {
        self.text || self.cursor || self.mode
    }
}

/// The edge of the text a vertical move tried to cross.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Edge {
    Top,
    Bottom,
}

pub(crate) type Keys = KeyManager<TerminalKey, Action<EditorInfo>, RepeatType>;

/// A text editor: Helix, Vim or modeless keys over one buffer, drawn at any width without the
/// text moving under the cursor.
pub struct Editor {
    pub(crate) buffer: EditBuffer<EditorInfo>,
    pub(crate) gid: CursorGroupId,
    pub(crate) store: Store<EditorInfo>,
    pub(crate) keys: Keys,
    pub(crate) keymap: KeymapKind,
    pub(crate) view: ViewportContext<Cursor>,
    pub(crate) prompt: Option<Prompt>,
    pub(crate) chips: Chips,
    pub(crate) events: Vec<EditorEvent>,
    /// The text as of the last action, and the rope it was made from.
    pub(crate) text: Text,
    /// The cursors and selections as of the last action.
    pub(crate) group: CursorGroup,
    rope: EditRope,
    revision: u64,
    /// The width the host last laid the editor out at.
    pub(crate) width: Cell<u16>,
    layout: RefCell<Option<(u64, u16, std::rc::Rc<Layout>)>>,
    /// First row shown.
    pub(crate) scroll: Cell<usize>,
    /// The column vertical moves aim for, kept across them.
    pub(crate) goal_column: Option<usize>,
    /// What the last typing action was, to group typing into undo steps when modeless.
    pub(crate) typing: crate::keys::Typing,
}

impl std::fmt::Debug for Editor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Editor")
            .field("keymap", &self.keymap)
            .field("text", &self.text.text)
            .finish_non_exhaustive()
    }
}

impl Editor {
    pub fn new(keymap: KeymapKind) -> Self {
        let mut buffer = EditBuffer::new("*nori-editor*".to_owned());
        let gid = buffer.create_group();
        let rope = buffer.get().clone();
        let view = ViewportContext {
            wrap: true,
            ..ViewportContext::default()
        };
        let mut editor = Editor {
            buffer,
            gid,
            store: Store::default(),
            keys: keymap.machine(),
            keymap,
            view,
            prompt: None,
            chips: Chips::default(),
            events: Vec::new(),
            text: Text::of_rope(&rope),
            group: CursorGroup::new(CursorState::Location(Cursor::default()), vec![]),
            rope,
            revision: 0,
            width: Cell::new(80),
            layout: RefCell::new(None),
            scroll: Cell::new(0),
            goal_column: None,
            typing: crate::keys::Typing::None,
        };
        editor.enter_insert();
        editor
    }

    pub fn keymap(&self) -> KeymapKind {
        self.keymap
    }

    /// Switch keymaps, keeping the text, the cursor and the undo history.
    pub fn set_keymap(&mut self, keymap: KeymapKind) {
        if self.keymap == keymap {
            return;
        }
        self.keymap = keymap;
        self.keys = keymap.machine();
        self.prompt = None;
        self.enter_insert();
    }

    /// The text, without chips expanded.
    pub fn text(&self) -> &str {
        &self.text.text
    }

    pub fn is_empty(&self) -> bool {
        self.text.text.is_empty()
    }

    /// Changes every time the text changes.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Replace the text, starting a new undo history, with the cursor at the end.
    pub fn set_text(&mut self, text: &str) {
        self.buffer.set_text(format!("{text}\n"));
        self.sync_text();
        self.set_cursor(self.text.text.len());
        self.scroll.set(0);
    }

    /// Replace the text as one undoable edit, with the cursor at `cursor`.
    pub fn replace_text(&mut self, text: &str, cursor: usize) {
        self.checkpoint();
        self.buffer.edit_text(format!("{text}\n"), &mut self.store);
        self.sync_text();
        self.set_cursor(cursor);
        self.checkpoint();
    }

    /// Replace `range` of the text with `with`, as one undoable edit; the cursor goes after it.
    pub fn replace_range(&mut self, range: Range<usize>, with: &str) {
        let text = &self.text.text;
        let start = range.start.min(text.len());
        let end = range.end.clamp(start, text.len());
        let new = format!("{}{with}{}", &text[..start], &text[end..]);
        self.replace_text(&new, start + with.len());
    }

    /// Insert `text` at every cursor (a paste), as one undoable edit.
    pub fn insert_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.checkpoint();
        let mut edits: Vec<(usize, &str)> = self
            .selections()
            .iter()
            .map(|selection| (selection.head, text))
            .collect();
        edits.sort_by_key(|(at, _)| *at);
        edits.dedup_by_key(|(at, _)| *at);
        self.apply_insertions(&edits);
        self.checkpoint();
    }

    /// Insert `text` at the primary cursor only, as one undoable edit.
    pub fn insert_at_cursor(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let at = self.cursor();
        self.checkpoint();
        self.apply_insertions(&[(at, text)]);
        self.checkpoint();
    }

    /// Insert a chip at the primary cursor (a chip stands for one thing: it is not repeated at
    /// every cursor) and return it.
    pub fn insert_chip(&mut self, chip: Chip) -> ChipId {
        let id = self.chips.add(chip);
        self.insert_at_cursor(&id.as_char().to_string());
        id
    }

    pub fn chip(&self, id: ChipId) -> Option<&Chip> {
        self.chips.get(id)
    }

    /// Byte offset of the primary cursor.
    pub fn cursor(&self) -> usize {
        self.primary().head
    }

    /// Put a single cursor at byte offset `at`, dropping every selection.
    pub fn set_cursor(&mut self, at: usize) {
        let cursor = self.text.cursor(at);
        self.buffer.set_group(
            self.gid,
            CursorGroup::new(CursorState::Location(cursor), vec![]),
        );
        self.goal_column = None;
        self.sync_text();
    }

    /// The primary selection.
    pub fn primary(&self) -> Selection {
        self.selection_of(&self.group.leader)
    }

    /// Every selection, the primary first.
    pub fn selections(&self) -> Vec<Selection> {
        let mut all = vec![self.selection_of(&self.group.leader)];
        all.extend(
            self.group
                .members
                .iter()
                .map(|state| self.selection_of(state)),
        );
        all
    }

    fn selection_of(&self, state: &CursorState) -> Selection {
        let head = self.text.offset(state.cursor());
        match state {
            CursorState::Location(_) => Selection {
                anchor: head,
                head,
                selected: false,
            },
            CursorState::Selection(_, anchor, shape) => {
                let anchor = self.text.offset(anchor);
                match shape {
                    TargetShape::LineWise => {
                        let (lo, hi) = (anchor.min(head), anchor.max(head));
                        let (start, _) = self.text.line_bounds(lo);
                        let (_, end) = self.text.line_bounds(hi);
                        if head >= anchor {
                            Selection {
                                anchor: start,
                                head: end,
                                selected: true,
                            }
                        } else {
                            Selection {
                                anchor: end,
                                head: start,
                                selected: true,
                            }
                        }
                    }
                    TargetShape::CharWise | TargetShape::BlockWise => Selection {
                        anchor,
                        head,
                        selected: true,
                    },
                }
            }
        }
    }

    /// The layout at `width`, cached until the text or the width changes.
    pub(crate) fn layout(&self, width: u16) -> std::rc::Rc<Layout> {
        self.width.set(width);
        let mut cache = self.layout.borrow_mut();
        if let Some((revision, cached_width, layout)) = cache.as_ref()
            && *revision == self.revision
            && *cached_width == width
        {
            return layout.clone();
        }
        let layout = std::rc::Rc::new(Layout::new(&self.text.text, &self.chips, width));
        *cache = Some((self.revision, width, layout.clone()));
        layout
    }

    /// Rows the text takes at `width` (the search prompt adds one while it is open).
    pub fn desired_height(&self, width: u16) -> u16 {
        let rows = self.layout(width).height() + usize::from(self.prompt.is_some());
        u16::try_from(rows).unwrap_or(u16::MAX)
    }

    /// The row the primary cursor is on at `width`, and how many rows there are.
    pub fn cursor_row(&self, width: u16) -> (usize, usize) {
        let layout = self.layout(width);
        let (row, _) = layout.position(&self.text.text, &self.chips, self.cursor());
        (row, layout.height())
    }

    /// Bring the text cache up to date with the buffer after an action.
    pub(crate) fn sync_text(&mut self) -> bool {
        self.group = self.buffer.cursor_group(self.gid);
        if &self.rope == self.buffer.get() {
            return false;
        }
        self.rope = self.buffer.get().clone();
        self.text = Text::of_rope(&self.rope);
        self.revision += 1;
        true
    }

    /// Make the current text an undo step of its own.
    pub(crate) fn checkpoint(&mut self) {
        let ctx = modalkit::editing::context::EditContext::default();
        let checkpoint =
            modalkit::actions::EditorAction::History(modalkit::actions::HistoryAction::Checkpoint);
        let _ = modalkit::actions::Editable::editor_command(
            &mut self.buffer,
            &checkpoint,
            &(self.gid, &self.view, &ctx),
            &mut self.store,
        );
    }

    /// Insert each text at its offset (sorted, distinct), leaving a cursor after each.
    pub(crate) fn apply_insertions(&mut self, edits: &[(usize, &str)]) {
        let mut new = String::with_capacity(self.text.text.len());
        let mut cursors = Vec::with_capacity(edits.len());
        let mut copied = 0;
        for (at, text) in edits {
            let at = (*at).clamp(copied, self.text.text.len());
            new.push_str(&self.text.text[copied..at]);
            new.push_str(text);
            cursors.push(new.len());
            copied = at;
        }
        new.push_str(&self.text.text[copied..]);
        self.buffer.edit_text(format!("{new}\n"), &mut self.store);
        self.sync_text();
        self.set_cursors(&cursors);
    }

    /// Replace every cursor and selection with `states`, the first the primary. Nothing changes
    /// when `states` is empty.
    pub(crate) fn set_states(&mut self, states: Vec<CursorState>) {
        let mut states = states.into_iter();
        if let Some(leader) = states.next() {
            self.buffer
                .set_group(self.gid, CursorGroup::new(leader, states.collect()));
            self.sync_text();
        }
    }

    /// One cursor at each offset, the first the primary.
    pub(crate) fn set_cursors(&mut self, offsets: &[usize]) {
        let mut states = offsets
            .iter()
            .map(|at| CursorState::Location(self.text.cursor(*at)));
        let Some(leader) = states.next() else {
            return;
        };
        self.buffer
            .set_group(self.gid, CursorGroup::new(leader, states.collect()));
        self.sync_text();
    }

    pub fn take_events(&mut self) -> Vec<EditorEvent> {
        std::mem::take(&mut self.events)
    }
}
