//! # Helix-like User Interfaces
//!
//! New for Nori (not part of upstream modalkit): a selection-first keymap that follows
//! [Helix](https://helix-editor.com)'s defaults, built on the same machinery as the Kakoune
//! environment ([super::kak]), from which this module's structure is derived.
//!
//! Helix selects first and acts second: motions such as `w` select what they move over, `x`
//! selects lines, `mi(` selects inside parentheses, and `d`, `c`, `y`, `r` act on every
//! selection. `v` toggles *select mode*, where motions extend the selections instead of replacing
//! them. Multiple selections come from `C`, `s` (select regex matches), `S` (split) and `alt-s`
//! (split lines).
//!
//! What Helix does that the editing model here cannot express directly (surround editing, the
//! space menu, language-server jumps) is produced as [HelixCommand]s inside
//! [Action::Application], for the application to carry out. An application opts in by making its
//! action type convertible from [HelixCommand].
//!
//! The keymap can also run *modeless*: Insert mode only, with `Esc` left to the application and
//! shifted motions extending the selection, for a conventional text field.
use std::marker::PhantomData;

use crate::{
    actions::{Action, EditAction, EditorAction, HistoryAction, InsertTextAction},
    editing::{
        application::{ApplicationInfo, EmptyInfo},
        context::{EditContext, EditContextBuilder},
        cursor::CursorStyle,
    },
    env::{CharacterContext, CommonKeyClass},
    key::TerminalKey,
    keybindings::{
        EdgeEvent,
        InputKey,
        InputKeyState,
        InputState,
        Mode,
        ModeKeys,
        ModeSequence,
        SequenceStatus,
    },
    prelude::*,
    util::{keycode_to_num, option_muladd_u32, option_muladd_usize},
};

pub mod keybindings;

/// What a Helix key asks of the application, beyond what the editing model can do itself.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HelixCommand {
    /// `ms<char>`: surround every selection with the pair `char` belongs to.
    SurroundAdd(char),
    /// `mr<from><to>`: replace the nearest enclosing `from` pair around each selection by `to`.
    SurroundReplace(char, char),
    /// `md<char>`: delete the nearest enclosing `char` pair around each selection.
    SurroundDelete(char),
    /// `w`, `b`, `e`, `W`, `B`, `E`: select to the next or previous word boundary (Helix's
    /// word motions select what they cross, end-exclusive, which the editing model's inclusive
    /// selections cannot express). With `true`, extend the selections instead (select mode).
    Word(WordMotion, bool),
    /// `x` (`true`): select the whole lines of each selection, or the next line too when they
    /// are already whole; `X` (`false`): stretch each selection to whole lines.
    SelectLines(bool),
    /// `>` (`true`) and `<` (`false`): indent or dedent every line a selection touches.
    Indent(bool),
    /// `*`: use the primary selection as the search pattern.
    SearchSelection,
    /// `:`: open the application's command palette.
    CommandPalette,
    /// The language-server jumps of goto mode (`gd`, `gy`, `gr`, `gi`) and the hover (`space k`).
    Lsp(LspCommand),
    /// `space y` / `space p` / `space P` / `space R`: the system clipboard.
    Clipboard(ClipboardCommand),
}

/// Where a word motion goes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WordMotion {
    /// `w`
    NextWordStart,
    /// `b`
    PrevWordStart,
    /// `e`
    NextWordEnd,
    /// `W`
    NextLongWordStart,
    /// `B`
    PrevLongWordStart,
    /// `E`
    NextLongWordEnd,
}

/// A language-server request made from the keymap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LspCommand {
    /// `gd`
    Definition,
    /// `gy`
    TypeDefinition,
    /// `gr`
    References,
    /// `gi`
    Implementation,
    /// `space k`
    Hover,
}

/// A system-clipboard operation made from the keymap.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipboardCommand {
    /// Copy the selections.
    Yank,
    /// Paste after the selections.
    PasteAfter,
    /// Paste before the selections.
    PasteBefore,
    /// Replace the selections with the clipboard.
    Replace,
}

impl From<HelixCommand> for () {
    fn from(_: HelixCommand) -> Self {}
}

/// Helix's input modes.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum HelixMode {
    /// Normal mode keypresses: motions replace the selections.
    #[default]
    Normal,

    /// Select mode keypresses: motions extend the selections (`v`).
    Select,

    /// Insert mode keypresses.
    Insert,

    /// Prompt mode keypresses (search, select and keep prompts).
    Prompt,

    /// Goto mode keypresses (`g`).
    Goto,

    /// Match mode keypresses (`m`).
    Match,

    /// View mode keypresses (`z`).
    View,

    /// Space mode keypresses (`<Space>`), where applications add their own keys.
    Space,

    /// The text object of `mi` / `ma`.
    ObjectSelect,
}

impl HelixMode {
    /// Whether this mode's motions extend the selections instead of replacing them.
    pub fn extends(&self) -> bool {
        matches!(self, HelixMode::Select)
    }
}

impl<I: ApplicationInfo> Mode<Action<I>, HelixState<I>> for HelixMode {
    fn enter(&self, prev: Self, ctx: &mut HelixState<I>) -> Vec<Action<I>> {
        match self {
            HelixMode::Normal => {
                ctx.persist.insert = None;
                ctx.persist.select = false;

                return vec![HistoryAction::Checkpoint.into()];
            },
            HelixMode::Select => {
                ctx.persist.insert = None;
                ctx.persist.select = true;

                return vec![];
            },
            HelixMode::Insert => {
                ctx.persist.insert = Some(InsertStyle::Insert);

                match prev {
                    HelixMode::Normal | HelixMode::Select | HelixMode::Insert => {
                        return vec![];
                    },
                    HelixMode::Prompt |
                    HelixMode::Goto |
                    HelixMode::Match |
                    HelixMode::View |
                    HelixMode::Space |
                    HelixMode::ObjectSelect => {
                        let action = EditAction::Motion.into();
                        let target = EditTarget::CurrentPosition;
                        let act = EditorAction::Edit(action, target);

                        return vec![act.into()];
                    },
                }
            },
            HelixMode::Prompt | HelixMode::Space => {
                ctx.persist.insert = Some(InsertStyle::Insert);

                return vec![];
            },
            HelixMode::View | HelixMode::ObjectSelect | HelixMode::Goto | HelixMode::Match => {
                ctx.persist.insert = None;

                return vec![];
            },
        }
    }

    fn show(&self, ctx: &HelixState<I>) -> Option<String> {
        let msg = match self {
            HelixMode::Normal | HelixMode::ObjectSelect => "",
            HelixMode::Prompt => "prompt",
            HelixMode::Select => "select",
            HelixMode::Goto => "goto",
            HelixMode::Match => "match",
            HelixMode::View => "view",
            HelixMode::Space => "space",
            HelixMode::Insert => "insert",
        };

        let mut res = String::from(msg);

        fn push(s: &mut String, suffix: String) {
            if !s.is_empty() {
                s.push(' ');
            }

            s.push_str(suffix.as_str());
        }

        if let Some(n) = ctx.action.count {
            push(&mut res, format!("count={n}"));
        }

        if let Some(r) = ctx.action.register.as_ref().and_then(register_to_char) {
            push(&mut res, format!("reg={r}"));
        }

        if !res.is_empty() {
            return Some(res);
        } else {
            return None;
        }
    }
}

impl<I: ApplicationInfo> ModeSequence<RepeatType, Action<I>, HelixState<I>> for HelixMode {
    fn sequences(
        &self,
        action: &Action<I>,
        ctx: &EditContext,
    ) -> Vec<(RepeatType, SequenceStatus)> {
        match self {
            HelixMode::Normal |
            HelixMode::Select |
            HelixMode::Insert |
            HelixMode::ObjectSelect |
            HelixMode::Space |
            HelixMode::View |
            HelixMode::Goto |
            HelixMode::Match => {
                vec![
                    (RepeatType::EditSequence, action.is_edit_sequence(SequenceStatus::Break, ctx)),
                    (RepeatType::LastAction, action.is_last_action(ctx)),
                    (RepeatType::LastSelection, action.is_last_selection(ctx)),
                ]
            },
            HelixMode::Prompt => {
                vec![]
            },
        }
    }
}

impl<I: ApplicationInfo> ModeKeys<TerminalKey, Action<I>, HelixState<I>> for HelixMode {
    fn unmapped(&self, ke: &TerminalKey, ctx: &mut HelixState<I>) -> (Vec<Action<I>>, Option<Self>) {
        match self {
            HelixMode::Normal | HelixMode::Select | HelixMode::View => {
                return (vec![], None);
            },
            HelixMode::Insert | HelixMode::Prompt => {
                if let Some(c) = ke.get_char() {
                    let ch = Char::Single(c).into();
                    let it = InsertTextAction::Type(ch, MoveDir1D::Previous, 1.into());

                    (vec![it.into()], None)
                } else {
                    (vec![], None)
                }
            },
            HelixMode::Space |
            HelixMode::ObjectSelect |
            HelixMode::Goto |
            HelixMode::Match => {
                let back = if ctx.persist.select { HelixMode::Select } else { HelixMode::Normal };

                (vec![], Some(back))
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ObjectPosition {
    Whole,
}

/// This is the context specific to an action, and gets reset every time a full sequence of
/// keybindings is pressed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ActionContext {
    // Fields for managing entered counts.
    pub(crate) count: Option<usize>,

    // Other arguments to key sequences.
    pub(crate) register: Option<Register>,
    pub(crate) register_update: RegisterUpdateStyle,

    // Where to place the cursor after performing an operation.
    pub(crate) cursor_end: CursorEnd,

    // Control object selection.
    pub(self) objsel: Option<(SelectionResizeStyle, ObjectPosition, bool)>,

    // The pair a `mr` replaces.
    pub(self) surround_from: Option<char>,

    // Control text selection.
    pub(crate) shape: Option<TargetShape>,

    // Cursor indicator to show on-screen.
    pub(crate) cursor: Option<char>,
}

impl Default for ActionContext {
    fn default() -> Self {
        Self {
            count: None,

            register: None,
            register_update: RegisterUpdateStyle::Replace,

            cursor_end: CursorEnd::Auto,

            objsel: None,
            surround_from: None,

            shape: None,

            cursor: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PersistentContext {
    pub(crate) regexsearch_dir: MoveDir1D,
    pub(crate) regexsearch_inc: bool,
    pub(crate) charsearch_params: (MoveDir1D, bool),
    pub(crate) charsearch: Option<Char>,
    pub(crate) insert: Option<InsertStyle>,
    /// Select mode is on: motions extend.
    pub(crate) select: bool,
}

impl Default for PersistentContext {
    fn default() -> Self {
        Self {
            regexsearch_dir: MoveDir1D::Next,
            regexsearch_inc: true,
            charsearch_params: (MoveDir1D::Next, false),
            charsearch: None,
            insert: None,
            select: false,
        }
    }
}

/// This wraps both action specific context, and persistent context.
#[derive(Debug, Eq, PartialEq)]
pub struct HelixState<I: ApplicationInfo = EmptyInfo> {
    pub(crate) action: ActionContext,
    pub(crate) persist: PersistentContext,
    pub(self) ch: CharacterContext,

    _p: PhantomData<I>,
}

impl<I: ApplicationInfo> Clone for HelixState<I> {
    fn clone(&self) -> Self {
        Self {
            action: self.action.clone(),
            persist: self.persist.clone(),
            ch: self.ch.clone(),

            _p: PhantomData,
        }
    }
}

impl<I: ApplicationInfo> Default for HelixState<I> {
    fn default() -> Self {
        HelixState {
            action: ActionContext::default(),
            persist: PersistentContext::default(),
            ch: CharacterContext::default(),

            _p: PhantomData,
        }
    }
}

impl<I: ApplicationInfo> InputState for HelixState<I> {
    type CursorHint = CursorStyle;
    type Output = EditContext;

    fn merge(original: EditContext, _: &EditContext) -> EditContext {
        original
    }

    fn reset(&mut self) {
        self.action = ActionContext::default();
    }

    fn take(&mut self) -> Self::Output {
        let ctx = Self {
            persist: self.persist.clone(),
            action: std::mem::take(&mut self.action),
            ch: std::mem::take(&mut self.ch),

            _p: PhantomData,
        };

        EditContext::from(ctx)
    }

    fn get_cursor_hint(&self) -> Self::CursorHint {
        CursorStyle {
            indicator: self.action.cursor,
            insert: self.persist.insert,
        }
    }
}

impl<I: ApplicationInfo> InputKeyState<TerminalKey, CommonKeyClass> for HelixState<I> {
    fn event(&mut self, ev: &EdgeEvent<TerminalKey, CommonKeyClass>, ke: &TerminalKey) {
        match ev {
            EdgeEvent::Key(_) | EdgeEvent::Fallthrough => {
                // Do nothing.
            },
            EdgeEvent::Class(CommonKeyClass::Mark) => {
                // Do nothing for now.
            },

            EdgeEvent::Class(CommonKeyClass::Count) => {
                if let Some(n) = keycode_to_num(ke, 10) {
                    let new = option_muladd_usize(&self.action.count, 10, n as usize);

                    self.action.count = Some(new);
                }
            },
            EdgeEvent::Class(CommonKeyClass::Register) => {
                if let Some((reg, style)) = key_to_register(ke) {
                    self.action.register = Some(reg);
                    self.action.register_update = style;
                }
            },

            // Track literals, codepoints, etc.
            EdgeEvent::Any => {
                self.ch.any = Some(*ke);
            },
            EdgeEvent::Class(CommonKeyClass::Octal) => {
                if let Some(n) = keycode_to_num(ke, 8) {
                    let new = option_muladd_u32(&self.ch.oct, 8, n);

                    self.ch.oct = Some(new);
                }
            },
            EdgeEvent::Class(CommonKeyClass::Decimal) => {
                if let Some(n) = keycode_to_num(ke, 10) {
                    let new = option_muladd_u32(&self.ch.dec, 10, n);

                    self.ch.dec = Some(new);
                }
            },
            EdgeEvent::Class(CommonKeyClass::Hexadecimal) => {
                if let Some(n) = keycode_to_num(ke, 16) {
                    let new = option_muladd_u32(&self.ch.hex, 16, n);

                    self.ch.hex = Some(new);
                }
            },
            EdgeEvent::Class(CommonKeyClass::Digraph1) => {
                if let Some(c) = ke.get_char() {
                    self.ch.digraph1 = Some(c);
                }
            },
            EdgeEvent::Class(CommonKeyClass::Digraph2) => {
                if let Some(c) = ke.get_char() {
                    self.ch.digraph2 = Some(c);
                }
            },
        }
    }
}

impl<I: ApplicationInfo> From<HelixState<I>> for EditContext {
    fn from(ctx: HelixState<I>) -> Self {
        let search_char = if let Some(c) = &ctx.persist.charsearch {
            let (dir, inc) = ctx.persist.charsearch_params;

            Some((dir, inc, c.clone()))
        } else {
            None
        };

        let typed = ctx.ch.get_typed();

        EditContextBuilder::default()
            .count(ctx.action.count)
            .typed_char(typed.clone())
            .cursor_end(ctx.action.cursor_end)
            .replace_char(typed)
            .search_char(search_char)
            .search_regex_dir(ctx.persist.regexsearch_dir)
            .target_shape(ctx.action.shape)
            .insert_style(ctx.persist.insert)
            .last_column(true)
            .register(ctx.action.register.clone())
            .register_update(ctx.action.register_update)
            .search_incremental(ctx.persist.regexsearch_inc)
            .build()
    }
}

fn register_to_char(reg: &Register) -> Option<char> {
    match reg {
        Register::Named(c) => (*c).into(),
        Register::Unnamed => '"'.into(),
        Register::UnnamedMacro => '@'.into(),
        Register::UnnamedCursorGroup => '^'.into(),
        Register::Blackhole => '_'.into(),
        Register::CurBufName => '%'.into(),
        Register::SelectionClipboard => '+'.into(),
        Register::SelectionPrimary => '*'.into(),
        Register::LastCommand(CommandType::Command) => ':'.into(),
        Register::LastCommand(CommandType::Search) => '/'.into(),
        Register::LastCommand(CommandType::Shell) => '|'.into(),

        _ => None,
    }
}

fn char_to_register(c: char) -> Option<(Register, RegisterUpdateStyle)> {
    let r = match c {
        // Lowercase letters
        c @ 'a'..='z' => Register::Named(c),

        // Uppercase letters append, as in Helix.
        c @ 'A'..='Z' => {
            return Some((Register::Named(c.to_ascii_lowercase()), RegisterUpdateStyle::Append));
        },

        // Special Characters
        '"' => Register::Unnamed,
        '@' => Register::UnnamedMacro,
        '^' => Register::UnnamedCursorGroup,

        '_' => Register::Blackhole,
        '%' => Register::CurBufName,
        '+' => Register::SelectionClipboard,
        '*' => Register::SelectionPrimary,
        ':' => Register::LastCommand(CommandType::Command),
        '/' => Register::LastCommand(CommandType::Search),

        _ => return None,
    };

    return Some((r, RegisterUpdateStyle::Replace));
}

fn key_to_register(ke: &TerminalKey) -> Option<(Register, RegisterUpdateStyle)> {
    char_to_register(ke.get_char()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mode_show() {
        let mut ctx: HelixState = HelixState::default();

        assert_eq!(HelixMode::Normal.show(&ctx), None);

        ctx.action.count = Some(5);
        assert_eq!(HelixMode::Normal.show(&ctx), Some("count=5".into()));

        ctx.action.register = Some(Register::Named('a'));
        assert_eq!(HelixMode::Normal.show(&ctx), Some("count=5 reg=a".into()));

        assert_eq!(HelixMode::Insert.show(&ctx), Some("insert count=5 reg=a".into()));
    }
}
