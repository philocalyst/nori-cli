//! # Helix Keybindings
//!
//! New for Nori (not part of upstream modalkit): Helix's default keymap mapped onto the
//! [Action] type, derived from the Kakoune environment's machinery ([crate::env::kak]).
//!
//! ## Example
//!
//! ```
//! use modalkit::env::helix::HelixMode;
//! use modalkit::env::helix::keybindings::{default_helix_keys, HelixMachine};
//!
//! use modalkit::keybindings::BindingMachine;
//! use modalkit::key::TerminalKey;
//!
//! use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
//!
//! let mut keybindings: HelixMachine<TerminalKey> = default_helix_keys();
//! assert_eq!(keybindings.mode(), HelixMode::Normal);
//!
//! keybindings.input_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE).into());
//! assert_eq!(keybindings.mode(), HelixMode::Select);
//! ```
use std::str::FromStr;

use bitflags::bitflags;

use editor_types::{
    Action,
    CommandBarAction,
    EditAction,
    EditorAction,
    SelectionAction,
    action,
    application::{ApplicationInfo, EmptyInfo},
};

use crate::{
    env::{CommonKeyClass, keyparse::parse},
    key::TerminalKey,
    keybindings::{InputBindings, ModalMachine, Step},
    prelude::*,
};

use super::{ClipboardCommand, HelixCommand, HelixMode, HelixState, LspCommand, ObjectPosition, WordMotion};

bitflags! {
    #[derive(Debug, Clone, Copy)]
    struct MappedModes: u32 {
        const N = 0b0000000000000001;
        const I = 0b0000000000000010;
        const P = 0b0000000000000100;
        const V = 0b0000000000001000;
        const O = 0b0000000000010000;
        const G = 0b0000000000100000;
        const S = 0b0000000001000000;
        const M = 0b0000000010000000;
        const U = 0b0000000100000000;

        const NS = Self::N.bits() | Self::S.bits();
        const IP = Self::I.bits() | Self::P.bits();
    }
}

/// Normal mode only.
const NMAP: MappedModes = MappedModes::N;
/// Select mode only.
const SMAP: MappedModes = MappedModes::S;
/// Normal and Select modes.
const NSMAP: MappedModes = MappedModes::NS;
const IMAP: MappedModes = MappedModes::I;
const PMAP: MappedModes = MappedModes::P;
const IPMAP: MappedModes = MappedModes::IP;
const VMAP: MappedModes = MappedModes::V;
const OMAP: MappedModes = MappedModes::O;
const GMAP: MappedModes = MappedModes::G;
const MMAP: MappedModes = MappedModes::M;
const UMAP: MappedModes = MappedModes::U;

impl MappedModes {
    pub fn split(&self) -> Vec<HelixMode> {
        let mut modes = Vec::new();

        if self.contains(MappedModes::N) {
            modes.push(HelixMode::Normal);
        }

        if self.contains(MappedModes::S) {
            modes.push(HelixMode::Select);
        }

        if self.contains(MappedModes::I) {
            modes.push(HelixMode::Insert);
        }

        if self.contains(MappedModes::P) {
            modes.push(HelixMode::Prompt);
        }

        if self.contains(MappedModes::V) {
            modes.push(HelixMode::View);
        }

        if self.contains(MappedModes::O) {
            modes.push(HelixMode::ObjectSelect);
        }

        if self.contains(MappedModes::G) {
            modes.push(HelixMode::Goto);
        }

        if self.contains(MappedModes::M) {
            modes.push(HelixMode::Match);
        }

        if self.contains(MappedModes::U) {
            modes.push(HelixMode::Space);
        }

        return modes;
    }
}

#[derive(Clone, Debug)]
enum InternalAction {
    SetCursorEnd(CursorEnd),
    SetObjectSelect(SelectionResizeStyle, ObjectPosition, bool),
    SetRegister(Register),
    SetSearchChar,
    SetSearchCharParams(MoveDir1D, bool),
    SetSurroundFrom,
    SetTargetShape(TargetShape),
}

impl InternalAction {
    pub fn run<I: ApplicationInfo>(&self, ctx: &mut HelixState<I>) {
        match self {
            InternalAction::SetCursorEnd(end) => {
                ctx.action.cursor_end = *end;
            },
            InternalAction::SetObjectSelect(style, pos, inc) => {
                ctx.action.objsel = Some((*style, *pos, *inc));
            },
            InternalAction::SetRegister(reg) => {
                ctx.action.register = Some(reg.clone());
            },
            InternalAction::SetSearchChar => {
                ctx.persist.charsearch = ctx.ch.get_typed();
            },
            InternalAction::SetSearchCharParams(dir, inclusive) => {
                ctx.persist.charsearch_params = (*dir, *inclusive);
            },
            InternalAction::SetSurroundFrom => {
                if let Some(Char::Single(c)) = ctx.ch.get_typed() {
                    ctx.action.surround_from = Some(c);
                }

                ctx.ch = Default::default();
            },
            InternalAction::SetTargetShape(shape) => {
                ctx.action.shape = Some(*shape);
            },
        }
    }
}

/// A surround edit, completed with the characters typed for it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Surround {
    Add,
    Replace,
    Delete,
}

#[derive(Debug, Eq, PartialEq)]
enum ExternalAction<I: ApplicationInfo> {
    CountOnly(Action<I>),
    ObjectSelect(RangeType, Option<RangeType>),
    Surround(Surround),
    Command(HelixCommand),
    Something(Action<I>),
}

impl<I: ApplicationInfo> ExternalAction<I>
where
    I::Action: From<HelixCommand>,
{
    fn resolve(&self, ctx: &mut HelixState<I>) -> Vec<Action<I>> {
        match self {
            ExternalAction::CountOnly(act) => {
                if ctx.action.count.is_some() {
                    vec![act.clone()]
                } else {
                    vec![]
                }
            },
            ExternalAction::ObjectSelect(rt1, rt2) => {
                if let Some((style, pos, inclusive)) = ctx.action.objsel {
                    let rt = match (rt2, inclusive) {
                        (Some(rt), true) => rt.clone(),
                        (_, _) => rt1.clone(),
                    };

                    let target = match pos {
                        ObjectPosition::Whole => {
                            EditTarget::Range(rt, inclusive, Count::Contextual)
                        },
                    };

                    vec![action!("selection resize -s {style} -t {target}")]
                } else {
                    vec![]
                }
            },
            ExternalAction::Surround(op) => {
                let Some(Char::Single(c)) = ctx.ch.get_typed() else {
                    return vec![];
                };

                let cmd = match (op, ctx.action.surround_from) {
                    (Surround::Add, _) => HelixCommand::SurroundAdd(c),
                    (Surround::Delete, _) => HelixCommand::SurroundDelete(c),
                    (Surround::Replace, Some(from)) => HelixCommand::SurroundReplace(from, c),
                    (Surround::Replace, None) => return vec![],
                };

                vec![Action::Application(cmd.into())]
            },
            ExternalAction::Command(cmd) => vec![Action::Application(cmd.clone().into())],
            ExternalAction::Something(act) => vec![act.clone()],
        }
    }
}

impl<I: ApplicationInfo> FromStr for InputStep<I> {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        let act = Action::from_str(s)?;
        Ok(Self::new().actions(vec![act]))
    }
}

impl<I: ApplicationInfo> Clone for ExternalAction<I> {
    fn clone(&self) -> Self {
        match self {
            ExternalAction::CountOnly(act) => ExternalAction::CountOnly(act.clone()),
            ExternalAction::ObjectSelect(range1, range2) => {
                ExternalAction::ObjectSelect(range1.clone(), range2.clone())
            },
            ExternalAction::Surround(op) => ExternalAction::Surround(*op),
            ExternalAction::Command(cmd) => ExternalAction::Command(cmd.clone()),
            ExternalAction::Something(act) => ExternalAction::Something(act.clone()),
        }
    }
}

impl<I: ApplicationInfo> From<Action<I>> for ExternalAction<I> {
    fn from(act: Action<I>) -> Self {
        ExternalAction::Something(act)
    }
}

/// Description of actions to take after an input sequence.
#[derive(Debug)]
pub struct InputStep<I: ApplicationInfo> {
    internal: Vec<InternalAction>,
    external: Vec<ExternalAction<I>>,
    fallthrough_mode: Option<HelixMode>,
    nextm: Option<HelixMode>,
}

impl<I: ApplicationInfo> InputStep<I> {
    /// Create a new step that input keys can map to.
    pub fn new() -> Self {
        InputStep { internal: vec![], external: vec![], fallthrough_mode: None, nextm: None }
    }

    /// Set the [actions](Action) that this step produces.
    pub fn actions(mut self, acts: Vec<Action<I>>) -> Self {
        self.external = acts.into_iter().map(ExternalAction::Something).collect();
        self
    }

    /// Set the mode the step moves to.
    pub fn goto(mut self, mode: HelixMode) -> Self {
        self.nextm = Some(mode);
        self
    }
}

impl<I: ApplicationInfo> Default for InputStep<I> {
    fn default() -> Self {
        Self::new()
    }
}

impl<I: ApplicationInfo> Clone for InputStep<I> {
    fn clone(&self) -> Self {
        Self {
            internal: self.internal.clone(),
            external: self.external.clone(),
            fallthrough_mode: self.fallthrough_mode,
            nextm: self.nextm,
        }
    }
}

impl<I: ApplicationInfo> Step<TerminalKey> for InputStep<I>
where
    I::Action: From<HelixCommand>,
{
    type A = Action<I>;
    type State = HelixState<I>;
    type M = HelixMode;
    type Class = CommonKeyClass;
    type Sequence = RepeatType;

    fn is_unmapped(&self) -> bool {
        match self {
            InputStep { internal, external, fallthrough_mode: None, nextm: None } => {
                internal.is_empty() && external.is_empty()
            },
            _ => false,
        }
    }

    fn fallthrough(&self) -> Option<Self::M> {
        self.fallthrough_mode
    }

    fn step(&self, ctx: &mut HelixState<I>) -> (Vec<Action<I>>, Option<Self::M>) {
        for iact in self.internal.iter() {
            iact.run(ctx);
        }

        let external: Vec<Action<I>> =
            self.external.iter().flat_map(|act| act.resolve(ctx)).collect();

        return (external, self.nextm);
    }
}

macro_rules! act {
    ($ext: expr) => {
        isv!(vec![], vec![ExternalAction::Something($ext)])
    };
    ($ext: expr, $ns: expr) => {
        isv!(vec![], vec![ExternalAction::Something($ext)], $ns)
    };
}

macro_rules! action_step {
    ($cmd: expr) => {
        act!(action!($cmd))
    };
    ($cmd: expr, $ns: expr) => {
        act!(action!($cmd), $ns)
    };
}

macro_rules! iact {
    ($int: expr) => {
        isv!(vec![$int], vec![])
    };
    ($int: expr, $ns: expr) => {
        isv!(vec![$int], vec![], $ns)
    };
}

macro_rules! isv {
    () => {
        InputStep { internal: vec![], external: vec![], fallthrough_mode: None, nextm: None }
    };
    ($ints: expr, $exts: expr) => {
        InputStep { internal: $ints, external: $exts, fallthrough_mode: None, nextm: None }
    };
    ($ints: expr, $exts: expr, $ns: expr) => {
        InputStep { internal: $ints, external: $exts, fallthrough_mode: None, nextm: Some($ns) }
    };
}

macro_rules! is {
    ($int: expr, $ext: expr) => {
        isv!(vec![$int], vec![ExternalAction::Something($ext.into())])
    };
    ($int: expr, $ext: expr, $ns: expr) => {
        isv!(vec![$int], vec![ExternalAction::Something($ext.into())], $ns)
    };
}

macro_rules! fallthrough {
    ($mode: expr) => {
        InputStep { internal: vec![], external: vec![], fallthrough_mode: Some($mode), nextm: None }
    };
    ($mode: expr, $iacts: expr) => {
        InputStep { internal: $iacts, external: vec![], fallthrough_mode: Some($mode), nextm: None }
    };
    ($mode: expr, $iacts: expr, $eacts: expr) => {
        InputStep { internal: $iacts, external: $eacts, fallthrough_mode: Some($mode), nextm: None }
    };
}

/// Delete what a motion covers into the unnamed register, so `<C-Y>` can put it back (the
/// readline kill and yank).
macro_rules! kill {
    ($mt: expr) => {
        editor!(EditorAction::Edit(
            EditAction::Delete(Specifier::Contextual, Specifier::Contextual).into(),
            EditTarget::Motion($mt, Count::Contextual)
        ))
    };
    ($mt: expr, $c: literal) => {
        editor!(EditorAction::Edit(
            EditAction::Delete(Specifier::Contextual, Specifier::Contextual).into(),
            EditTarget::Motion($mt, Count::Exact($c))
        ))
    };
}

macro_rules! command {
    ($cmd: expr) => {
        isv!(vec![], vec![ExternalAction::Command($cmd)])
    };
}

macro_rules! surround {
    ($op: expr) => {
        isv!(vec![], vec![ExternalAction::Surround($op)])
    };
}

macro_rules! goto_goto {
    ($iacts: expr) => {
        fallthrough!(HelixMode::Goto, $iacts, vec![ExternalAction::CountOnly(
            EditorAction::Edit(
                Specifier::Exact(EditAction::Motion),
                MoveType::BufferLineOffset.into()
            )
            .into()
        )])
    };
}

macro_rules! shaped {
    ($shape: expr, $act: expr) => {
        is!(InternalAction::SetTargetShape($shape), $act)
    };
}

macro_rules! blackhole {
    ($act: expr) => {
        is!(InternalAction::SetRegister(Register::Blackhole), $act)
    };
    ($act: expr, $nm: expr) => {
        is!(InternalAction::SetRegister(Register::Blackhole), $act, $nm)
    };
}

macro_rules! insert {
    ($mt: expr, $c: literal) => {
        edit!(EditAction::Motion, $mt, Count::Exact(0), HelixMode::Insert)
    };
}

macro_rules! edit_target_cursor_end {
    ($ea: expr, $et: expr, $end: expr) => {
        is!(InternalAction::SetCursorEnd($end), EditorAction::Edit(Specifier::Exact($ea), $et))
    };
}

macro_rules! edit_selection_cursor_keep {
    ($ea: expr) => {
        edit_target_cursor_end!($ea, EditTarget::Selection, CursorEnd::Keep)
    };
}

macro_rules! action_selection {
    ($cmd: expr) => {
        is!(InternalAction::SetCursorEnd(CursorEnd::Selection), action!($cmd))
    };
}

macro_rules! open_lines {
    ($dir: expr) => {
        isv!(
            vec![],
            vec![
                ExternalAction::Something(action!("cursor split -c ctx-sub-one")),
                ExternalAction::Something(action!("insert open-line -S line -d {} -c 1", $dir))
            ],
            HelixMode::Insert
        )
    };
}

macro_rules! selection_resize {
    ($style: expr, $et: expr) => {
        shaped!(
            TargetShape::CharWise,
            EditorAction::Selection(SelectionAction::Resize($style, $et))
        )
    };
}

macro_rules! extend_target {
    ($et: expr) => {
        selection_resize!(SelectionResizeStyle::Extend, $et)
    };
}

macro_rules! extend {
    ($mt: expr) => {
        extend_target!(EditTarget::Motion($mt, Count::Contextual))
    };
    ($mt: expr, $c: literal) => {
        extend_target!(EditTarget::Motion($mt, Count::Exact($c)))
    };
}

macro_rules! extend_search {
    ($st: expr, $mod: expr) => {
        is!(
            InternalAction::SetSearchChar,
            EditorAction::Selection(SelectionAction::Resize(
                SelectionResizeStyle::Extend,
                EditTarget::Search($st, $mod, Count::Contextual)
            ))
        )
    };
}

macro_rules! selection_restart_target {
    ($et: expr) => {
        selection_resize!(SelectionResizeStyle::Restart, $et)
    };
}

macro_rules! selection_restart_search {
    ($st: expr, $mod: expr) => {
        is!(
            InternalAction::SetSearchChar,
            EditorAction::Selection(SelectionAction::Resize(
                SelectionResizeStyle::Restart,
                EditTarget::Search($st, $mod, Count::Contextual)
            ))
        )
    };
}

macro_rules! selection_object_search {
    ($dir: expr) => {
        selection_resize!(
            SelectionResizeStyle::Object,
            EditTarget::Search(SearchType::Regex, MoveDirMod::Exact($dir), Count::Contextual)
        )
    };
}

/// Open the search prompt; Enter selects the next match in `$dir` of what was typed.
macro_rules! search_prompt {
    ($label: expr, $dir: expr) => {
        act!(
            Action::CommandBar(CommandBarAction::Focus(
                $label.into(),
                CommandType::Search,
                Box::new(
                    EditorAction::Selection(SelectionAction::Resize(
                        SelectionResizeStyle::Object,
                        EditTarget::Search(SearchType::Regex, MoveDirMod::Exact($dir), Count::Contextual)
                    ))
                    .into()
                )
            )),
            HelixMode::Prompt
        )
    };
}

macro_rules! object_select {
    ($style: expr, $pos: expr, $inc: expr) => {
        fallthrough!(HelixMode::ObjectSelect, vec![
            InternalAction::SetTargetShape(TargetShape::CharWise),
            InternalAction::SetObjectSelect($style, $pos, $inc)
        ])
    };
}

macro_rules! object_end {
    ($rt: expr) => {
        isv!(vec![], vec![ExternalAction::ObjectSelect($rt, None)], HelixMode::Normal)
    };
    ($rt1: expr, $rt2: expr) => {
        isv!(vec![], vec![ExternalAction::ObjectSelect($rt1, Some($rt2))], HelixMode::Normal)
    };
}

macro_rules! delete_selection {
    () => {
        editor!(EditorAction::Edit(
            Specifier::Exact(EditAction::Delete(Specifier::Contextual, Specifier::Contextual)),
            EditTarget::Selection
        ), HelixMode::Normal)
    };
    ($nm: expr) => {
        editor!(
            EditorAction::Edit(
                Specifier::Exact(EditAction::Delete(Specifier::Contextual, Specifier::Contextual)),
                EditTarget::Selection
            ),
            $nm
        )
    };
    ($nm: expr, $register: expr) => {
        is!(
            InternalAction::SetRegister($register),
            EditorAction::Edit(
                Specifier::Exact(EditAction::Delete(Specifier::Contextual, Specifier::Contextual)),
                EditTarget::Selection
            ),
            $nm
        )
    };
}

/// Every motion of Normal and Select mode: in Normal mode it replaces the selections with what it
/// moves over (or, for the character and line motions, with the new position); in Select mode it
/// extends them.
#[rustfmt::skip]
fn motion_keys<I: ApplicationInfo>() -> Vec<(MappedModes, &'static str, InputStep<I>)> {
    use MoveDir1D::{Next, Previous};

    let mut keys = vec![];

    // Character and line motions move the cursor in Normal mode.
    for (key, mt) in [
        ("h", MoveType::Column(Previous, true)),
        ("<Left>", MoveType::Column(Previous, true)),
        ("l", MoveType::Column(Next, true)),
        ("<Right>", MoveType::Column(Next, true)),
        ("j", MoveType::ScreenLine(Next)),
        ("<Down>", MoveType::ScreenLine(Next)),
        ("k", MoveType::ScreenLine(Previous)),
        ("<Up>", MoveType::ScreenLine(Previous)),
    ] {
        keys.push((NMAP, key, edit!(EditAction::Motion, mt.clone())));
        keys.push((SMAP, key, extend!(mt)));
    }

    for (key, mt) in [
        ("<Home>", MoveType::LinePos(MovePosition::Beginning)),
        ("<End>", MoveType::LinePos(MovePosition::End)),
    ] {
        keys.push((NMAP, key, edit!(EditAction::Motion, mt.clone(), 0)));
        keys.push((SMAP, key, extend!(mt, 0)));
    }

    // Word motions select what they move over; the application carries them out.
    for (key, motion) in [
        ("w", WordMotion::NextWordStart),
        ("b", WordMotion::PrevWordStart),
        ("e", WordMotion::NextWordEnd),
        ("W", WordMotion::NextLongWordStart),
        ("B", WordMotion::PrevLongWordStart),
        ("E", WordMotion::NextLongWordEnd),
    ] {
        keys.push((NMAP, key, command!(HelixCommand::Word(motion, false))));
        keys.push((SMAP, key, command!(HelixCommand::Word(motion, true))));
    }

    // Character search: f, t, F, T, then the character.
    for (key, dir, inclusive) in [("f", Next, true), ("t", Next, false), ("F", Previous, true), ("T", Previous, false)] {
        keys.push((NSMAP, key, iact!(InternalAction::SetSearchCharParams(dir, inclusive))));
    }

    for key in ["f{any}", "t{any}", "F{any}", "T{any}"] {
        keys.push((NMAP, key, selection_restart_search!(SearchType::Char(true), MoveDirMod::Same)));
        keys.push((SMAP, key, extend_search!(SearchType::Char(true), MoveDirMod::Same)));
    }

    keys
}

#[rustfmt::skip]
fn default_keys<I: ApplicationInfo>() -> Vec<(MappedModes, &'static str, InputStep<I>)> {
    let mut keys = motion_keys();

    keys.extend([
        // Normal and Select mode keys.
        ( NSMAP, "<A-.>", action_step!("repeat -s last-selection") ),
        ( NSMAP, "G", goto_goto!(vec![]) ),
        ( NSMAP, "g", goto_goto!(vec![]) ),
        ( NSMAP, "i", action_step!("selection cursor-set -f beginning", HelixMode::Insert) ),
        // After the selection: its last character is part of it, so the cursor steps past it
        // (onto the line end too: the step is taken in Insert, where the cursor may go there).
        ( NSMAP, "a", isv!(vec![], vec![
            ExternalAction::Something(action!("selection cursor-set -f end")),
            ExternalAction::Something(
                EditorAction::Edit(
                    Specifier::Exact(EditAction::Motion),
                    EditTarget::Motion(MoveType::Column(MoveDir1D::Next, false), Count::Exact(1)),
                )
                .into(),
            ),
        ], HelixMode::Insert) ),
        ( NSMAP, "I", insert!(MoveType::FirstWord(MoveDir1D::Next), 0) ),
        ( NSMAP, "A", insert!(MoveType::LinePos(MovePosition::End), 0) ),
        ( NSMAP, "o", open_lines!(MoveDir1D::Next) ),
        ( NSMAP, "O", open_lines!(MoveDir1D::Previous) ),
        ( NSMAP, "d", delete_selection!() ),
        ( NSMAP, "<A-d>", delete_selection!(HelixMode::Normal, Register::Blackhole) ),
        ( NSMAP, "c", delete_selection!(HelixMode::Insert) ),
        ( NSMAP, "<A-c>", delete_selection!(HelixMode::Insert, Register::Blackhole) ),
        ( NSMAP, "y", edit_selection!(EditAction::Yank(Specifier::Contextual, Specifier::Contextual), HelixMode::Normal) ),
        ( NSMAP, "p", action_selection!("insert paste -s (side -d next)") ),
        ( NSMAP, "P", action_selection!("insert paste -s (side -d previous)") ),
        ( NSMAP, "R", action_selection!("insert paste -s replace") ),
        ( NSMAP, "r{any}", edit_selection!(EditAction::Replace(false), HelixMode::Normal) ),
        ( NSMAP, "u", action_step!("history undo -c ctx", HelixMode::Normal) ),
        ( NSMAP, "U", action_step!("history redo -c ctx", HelixMode::Normal) ),
        ( NSMAP, "<A-u>", action_step!("history undo -c ctx", HelixMode::Normal) ),
        ( NSMAP, "<A-U>", action_step!("history redo -c ctx", HelixMode::Normal) ),
        ( NSMAP, ".", action_step!("repeat -s edit-sequence") ),
        ( NSMAP, "~", edit_selection!(EditAction::ChangeCase(Case::Toggle), HelixMode::Normal) ),
        ( NSMAP, "`", edit_selection!(EditAction::ChangeCase(Case::Lower), HelixMode::Normal) ),
        ( NSMAP, "<A-`>", edit_selection!(EditAction::ChangeCase(Case::Upper), HelixMode::Normal) ),
        ( NSMAP, ">", command!(HelixCommand::Indent(true)) ),
        ( NSMAP, "<", command!(HelixCommand::Indent(false)) ),
        ( NSMAP, "J", edit_selection_cursor_keep!(EditAction::Join(JoinStyle::OneSpace)) ),
        ( NSMAP, "<A-J>", edit_selection_cursor_keep!(EditAction::Join(JoinStyle::NoChange)) ),
        ( NSMAP, "<C-A>", edit_selection!(EditAction::ChangeNumber(NumberChange::Increase(Count::Contextual), false)) ),
        ( NSMAP, "<C-X>", edit_selection!(EditAction::ChangeNumber(NumberChange::Decrease(Count::Contextual), false)) ),

        // Selections.
        ( NSMAP, "x", command!(HelixCommand::SelectLines(true)) ),
        ( NSMAP, "X", command!(HelixCommand::SelectLines(false)) ),
        ( NSMAP, "<A-x>", action_step!("selection trim -b line -t all") ),
        ( NSMAP, "%", selection_restart_target!(RangeType::Buffer.into()) ),
        ( NSMAP, ";", selection_restart_target!(EditTarget::CurrentPosition) ),
        ( NSMAP, "<A-;>", action_step!("selection cursor-set -f swap-anchor") ),
        ( NSMAP, "<A-:>", action_step!("selection cursor-set -f end") ),
        ( NSMAP, ",", action_step!("cursor close -t followers") ),
        ( NSMAP, "<A-,>", action_step!("cursor close -t leader") ),
        ( NSMAP, "C", action_step!("selection duplicate -d next -c ctx") ),
        ( NSMAP, "<A-C>", action_step!("selection duplicate -d previous -c ctx") ),
        ( NSMAP, "s", action_step!(r#"cmdbar focus -P "select:" -s search -a (selection split -s (regex keep) -M all)"#, HelixMode::Prompt) ),
        ( NSMAP, "S", action_step!(r#"cmdbar focus -P "split:" -s search -a (selection split -s (regex drop) -M all)"#, HelixMode::Prompt) ),
        ( NSMAP, "<A-s>", action_step!("selection split -s lines") ),
        ( NSMAP, "<A-_>", action_step!("selection join") ),
        ( NSMAP, "K", action_step!(r#"cmdbar focus -P "keep:" -s search -a (selection filter -M keep)"#, HelixMode::Prompt) ),
        ( NSMAP, "<A-K>", action_step!(r#"cmdbar focus -P "remove:" -s search -a (selection filter -M drop)"#, HelixMode::Prompt) ),
        ( NSMAP, "_", action_step!("selection trim -b non-whitespace -t all") ),
        ( NSMAP, ")", action_step!("cursor rotate -d next -c ctx") ),
        ( NSMAP, "(", action_step!("cursor rotate -d previous -c ctx") ),
        ( NSMAP, "&", unmapped!() ),
        ( NSMAP, "=", unmapped!() ),

        // Search.
        ( NSMAP, "/", search_prompt!("search:", MoveDir1D::Next) ),
        ( NSMAP, "?", search_prompt!("reverse search:", MoveDir1D::Previous) ),
        ( NMAP, "n", selection_object_search!(MoveDir1D::Next) ),
        ( NMAP, "N", selection_object_search!(MoveDir1D::Previous) ),
        ( SMAP, "n", extend_search!(SearchType::Regex, MoveDirMod::Exact(MoveDir1D::Next)) ),
        ( SMAP, "N", extend_search!(SearchType::Regex, MoveDirMod::Exact(MoveDir1D::Previous)) ),
        ( NSMAP, "*", command!(HelixCommand::SearchSelection) ),

        // Macros and registers.
        ( NSMAP, "Q", action_step!("macro toggle-recording") ),
        ( NSMAP, "q", action_step!("macro execute -c ctx") ),

        // Jumps and scrolling.
        ( NSMAP, "<C-I>", action_step!("jump -t jump-list -d next -c ctx") ),
        ( NSMAP, "<C-O>", action_step!("jump -t jump-list -d prev -c ctx") ),
        ( NSMAP, "<C-U>", action_step!("scroll -s (dir2d -d up -z half-page)") ),
        ( NSMAP, "<C-D>", action_step!("scroll -s (dir2d -d down -z half-page)") ),
        ( NSMAP, "<C-B>", action_step!("scroll -s (dir2d -d up -z page)") ),
        ( NSMAP, "<C-F>", action_step!("scroll -s (dir2d -d down -z page)") ),
        ( NSMAP, "<PageUp>", action_step!("scroll -s (dir2d -d up -z page)") ),
        ( NSMAP, "<PageDown>", action_step!("scroll -s (dir2d -d down -z page)") ),

        // Mode changes.
        ( NMAP, "v", goto!(HelixMode::Select) ),
        ( SMAP, "v", goto!(HelixMode::Normal) ),
        ( SMAP, "<Esc>", goto!(HelixMode::Normal) ),
        ( NSMAP, "m", fallthrough!(HelixMode::Match) ),
        ( NSMAP, "z", fallthrough!(HelixMode::View) ),
        ( NSMAP, "Z", goto!(HelixMode::View) ),
        ( NSMAP, "<Space>", fallthrough!(HelixMode::Space) ),
        ( NSMAP, ":", command!(HelixCommand::CommandPalette) ),

        // Goto mode keys.
        ( GMAP, "g", edit!(EditAction::Motion, MoveType::BufferPos(MovePosition::Beginning)) ),
        ( GMAP, "e", edit_buffer!(EditAction::Motion, MoveTerminus::End) ),
        ( GMAP, "h", edit!(EditAction::Motion, MoveType::LinePos(MovePosition::Beginning), 0) ),
        ( GMAP, "l", edit!(EditAction::Motion, MoveType::LinePos(MovePosition::End), 0) ),
        ( GMAP, "s", edit!(EditAction::Motion, MoveType::FirstWord(MoveDir1D::Next), 0) ),
        ( GMAP, "t", edit!(EditAction::Motion, MoveType::ViewportPos(MovePosition::Beginning)) ),
        ( GMAP, "c", edit!(EditAction::Motion, MoveType::ViewportPos(MovePosition::Middle)) ),
        ( GMAP, "b", edit!(EditAction::Motion, MoveType::ViewportPos(MovePosition::End)) ),
        ( GMAP, "j", edit!(EditAction::Motion, MoveType::Line(MoveDir1D::Next)) ),
        ( GMAP, "k", edit!(EditAction::Motion, MoveType::Line(MoveDir1D::Previous)) ),
        ( GMAP, ".", edit_target!(EditAction::Motion, EditTarget::CharJump(Mark::LastInserted.into())) ),
        ( GMAP, "d", command!(HelixCommand::Lsp(LspCommand::Definition)) ),
        ( GMAP, "y", command!(HelixCommand::Lsp(LspCommand::TypeDefinition)) ),
        ( GMAP, "r", command!(HelixCommand::Lsp(LspCommand::References)) ),
        ( GMAP, "i", command!(HelixCommand::Lsp(LspCommand::Implementation)) ),

        // Match mode keys.
        ( MMAP, "m", selection_resize!(SelectionResizeStyle::Object, RangeType::Item.into()) ),
        ( MMAP, "i", object_select!(SelectionResizeStyle::Object, ObjectPosition::Whole, false) ),
        ( MMAP, "a", object_select!(SelectionResizeStyle::Object, ObjectPosition::Whole, true) ),
        ( MMAP, "s{any}", surround!(Surround::Add) ),
        ( MMAP, "d{any}", surround!(Surround::Delete) ),
        ( MMAP, "r{any}", iact!(InternalAction::SetSurroundFrom) ),
        ( MMAP, "r{any}{any}", surround!(Surround::Replace) ),

        // View mode keys.
        ( VMAP, "<Esc>", goto!(HelixMode::Normal) ),
        ( VMAP, "z", action_step!("scroll -s (cursor-pos -p middle -x vertical)") ),
        ( VMAP, "c", action_step!("scroll -s (cursor-pos -p middle -x vertical)") ),
        ( VMAP, "t", action_step!("scroll -s (cursor-pos -p beginning -x vertical)") ),
        ( VMAP, "b", action_step!("scroll -s (cursor-pos -p end -x vertical)") ),
        ( VMAP, "j", action_step!("scroll -s (dir2d -d down -z cell)") ),
        ( VMAP, "k", action_step!("scroll -s (dir2d -d up -z cell)") ),

        // Space mode keys: the system clipboard and the hover; applications add their own.
        ( UMAP, "y", command!(HelixCommand::Clipboard(ClipboardCommand::Yank)) ),
        ( UMAP, "p", command!(HelixCommand::Clipboard(ClipboardCommand::PasteAfter)) ),
        ( UMAP, "P", command!(HelixCommand::Clipboard(ClipboardCommand::PasteBefore)) ),
        ( UMAP, "R", command!(HelixCommand::Clipboard(ClipboardCommand::Replace)) ),
        ( UMAP, "k", command!(HelixCommand::Lsp(LspCommand::Hover)) ),

        // Insert and Prompt mode keys.
        ( IPMAP, "<C-R>{register}", action_step!("insert paste -s cursor") ),
        ( IPMAP, "<Home>", edit!(EditAction::Motion, MoveType::LinePos(MovePosition::Beginning), 0) ),
        ( IPMAP, "<End>", edit!(EditAction::Motion, MoveType::LinePos(MovePosition::End), 0) ),
        ( IPMAP, "<C-A>", edit!(EditAction::Motion, MoveType::LinePos(MovePosition::Beginning), 0) ),
        ( IPMAP, "<C-E>", edit!(EditAction::Motion, MoveType::LinePos(MovePosition::End), 0) ),
        ( IPMAP, "<Left>", edit!(EditAction::Motion, MoveType::Column(MoveDir1D::Previous, true)) ),
        ( IPMAP, "<Right>", edit!(EditAction::Motion, MoveType::Column(MoveDir1D::Next, true)) ),
        ( IPMAP, "<C-B>", edit!(EditAction::Motion, MoveType::Column(MoveDir1D::Previous, true)) ),
        ( IPMAP, "<C-F>", edit!(EditAction::Motion, MoveType::Column(MoveDir1D::Next, true)) ),
        ( IPMAP, "<A-b>", edit!(EditAction::Motion, MoveType::WordBegin(WordStyle::Little, MoveDir1D::Previous)) ),
        ( IPMAP, "<A-f>", edit!(EditAction::Motion, MoveType::WordBegin(WordStyle::Little, MoveDir1D::Next)) ),
        ( IPMAP, "<C-Left>", edit!(EditAction::Motion, MoveType::WordBegin(WordStyle::Little, MoveDir1D::Previous)) ),
        ( IPMAP, "<C-Right>", edit!(EditAction::Motion, MoveType::WordBegin(WordStyle::Little, MoveDir1D::Next)) ),
        ( IPMAP, "<A-Left>", edit!(EditAction::Motion, MoveType::WordBegin(WordStyle::Little, MoveDir1D::Previous)) ),
        ( IPMAP, "<A-Right>", edit!(EditAction::Motion, MoveType::WordBegin(WordStyle::Little, MoveDir1D::Next)) ),
        ( IPMAP, "<BS>", erase!(MoveType::Column(MoveDir1D::Previous, true)) ),
        ( IPMAP, "<C-H>", erase!(MoveType::Column(MoveDir1D::Previous, true)) ),
        ( IPMAP, "<Del>", erase!(MoveType::Column(MoveDir1D::Next, true)) ),
        ( IPMAP, "<C-D>", erase!(MoveType::Column(MoveDir1D::Next, true)) ),
        ( IPMAP, "<C-W>", kill!(MoveType::WordBegin(WordStyle::Little, MoveDir1D::Previous)) ),
        ( IPMAP, "<A-BS>", kill!(MoveType::WordBegin(WordStyle::Little, MoveDir1D::Previous)) ),
        ( IPMAP, "<A-d>", kill!(MoveType::WordEnd(WordStyle::Little, MoveDir1D::Next)) ),
        ( IPMAP, "<A-Del>", kill!(MoveType::WordEnd(WordStyle::Little, MoveDir1D::Next)) ),
        ( IPMAP, "<C-U>", kill!(MoveType::LinePos(MovePosition::Beginning), 0) ),
        ( IPMAP, "<C-K>", kill!(MoveType::LinePos(MovePosition::End), 0) ),

        ( IPMAP, "<C-Y>", action_step!("insert paste -s cursor") ),

        // Insert mode keys.
        ( IMAP, "<Esc>", goto!(HelixMode::Normal) ),
        ( IMAP, "<C-S>", action_step!("history checkpoint") ),
        ( IMAP, "<C-J>", action_step!("insert type -i (exact '\\n') -d prev -c 1") ),
        ( IMAP, "<Up>", edit!(EditAction::Motion, MoveType::ScreenLine(MoveDir1D::Previous)) ),
        ( IMAP, "<Down>", edit!(EditAction::Motion, MoveType::ScreenLine(MoveDir1D::Next)) ),

        // Prompt mode keys.
        ( PMAP, "<Esc>", action_step!("prompt abort", HelixMode::Normal) ),
        ( PMAP, "<Enter>", action_step!("prompt submit", HelixMode::Normal) ),
        ( PMAP, "<Up>", action_step!("prompt recall -d previous -c ctx -M all") ),
        ( PMAP, "<Down>", action_step!("prompt recall -d next -c ctx -M all") ),

        // Text objects of `mi` and `ma`.
        ( OMAP, "w", object_end!(RangeType::Word(WordStyle::Little)) ),
        ( OMAP, "W", object_end!(RangeType::Word(WordStyle::Big)) ),
        ( OMAP, "p", object_end!(RangeType::Paragraph) ),
        ( OMAP, "s", object_end!(RangeType::Sentence) ),
        ( OMAP, "n", object_end!(RangeType::Word(WordStyle::Number(Radix::Decimal))) ),
        ( OMAP, "(", object_end!(RangeType::Bracketed('(', ')')) ),
        ( OMAP, ")", object_end!(RangeType::Bracketed('(', ')')) ),
        ( OMAP, "b", object_end!(RangeType::Bracketed('(', ')')) ),
        ( OMAP, "[", object_end!(RangeType::Bracketed('[', ']')) ),
        ( OMAP, "]", object_end!(RangeType::Bracketed('[', ']')) ),
        ( OMAP, "{", object_end!(RangeType::Bracketed('{', '}')) ),
        ( OMAP, "}", object_end!(RangeType::Bracketed('{', '}')) ),
        ( OMAP, "B", object_end!(RangeType::Bracketed('{', '}')) ),
        ( OMAP, "<", object_end!(RangeType::Bracketed('<', '>')) ),
        ( OMAP, ">", object_end!(RangeType::Bracketed('<', '>')) ),
        ( OMAP, "'", object_end!(RangeType::Quote('\'')) ),
        ( OMAP, "\"", object_end!(RangeType::Quote('"')) ),
        ( OMAP, "`", object_end!(RangeType::Quote('`')) ),
        ( OMAP, "q", object_end!(RangeType::Quote('"')) ),
        ( OMAP, "m", object_end!(RangeType::Item) ),
    ]);

    keys
}

#[rustfmt::skip]
fn default_pfxs<I: ApplicationInfo>() -> Vec<(MappedModes, &'static str, Option<InputStep<I>>)> {
    [
        // Normal and Select mode commands can be prefixed with a count and a register.
        ( NSMAP, "{count}", None ),
        ( NSMAP, "\"{register}", None ),
    ].to_vec()
}

#[rustfmt::skip]
fn default_enter<I: ApplicationInfo>() -> Vec<(MappedModes, &'static str, InputStep<I>)> {
    [
        // <Enter> in Insert mode types a newline character.
        ( IMAP, "<Enter>", action_step!("insert type -i (exact '\\n') -d prev -c 1") ),
    ].to_vec()
}

/// Keys of a modeless editor: shifted motions extend the selection, `Ctrl-Z` undoes, and `Esc`
/// is left to the application.
#[rustfmt::skip]
fn modeless_keys<I: ApplicationInfo>() -> Vec<(MappedModes, &'static str, InputStep<I>)> {
    [
        ( IMAP, "<Esc>", unmapped!() ),
        ( IMAP, "<S-Left>", extend!(MoveType::Column(MoveDir1D::Previous, true)) ),
        ( IMAP, "<S-Right>", extend!(MoveType::Column(MoveDir1D::Next, true)) ),
        ( IMAP, "<S-Up>", extend!(MoveType::ScreenLine(MoveDir1D::Previous)) ),
        ( IMAP, "<S-Down>", extend!(MoveType::ScreenLine(MoveDir1D::Next)) ),
        ( IMAP, "<S-Home>", extend!(MoveType::LinePos(MovePosition::Beginning), 0) ),
        ( IMAP, "<S-End>", extend!(MoveType::LinePos(MovePosition::End), 0) ),
        ( IMAP, "<C-S-Left>", extend!(MoveType::WordBegin(WordStyle::Little, MoveDir1D::Previous)) ),
        ( IMAP, "<C-S-Right>", extend!(MoveType::WordBegin(WordStyle::Little, MoveDir1D::Next)) ),
        ( IMAP, "<A-S-Left>", extend!(MoveType::WordBegin(WordStyle::Little, MoveDir1D::Previous)) ),
        ( IMAP, "<A-S-Right>", extend!(MoveType::WordBegin(WordStyle::Little, MoveDir1D::Next)) ),
        ( IMAP, "<C-Z>", action_step!("history undo -c ctx") ),
        ( IMAP, "<C-_>", action_step!("history undo -c ctx") ),
        ( IMAP, "<C-S-Z>", action_step!("history redo -c ctx") ),
        ( IMAP, "<A-z>", action_step!("history redo -c ctx") ),
    ].to_vec()
}

#[inline]
fn add_prefix<I: ApplicationInfo>(
    machine: &mut HelixMachine<TerminalKey, I>,
    modes: &MappedModes,
    keys: &str,
    action: &Option<InputStep<I>>,
) where
    I::Action: From<HelixCommand>,
{
    let (_, evs) = parse(keys).unwrap_or_else(|_| panic!("invalid helix keybinding: {keys}"));
    let modes = modes.split();

    for mode in modes {
        machine.add_prefix(mode, &evs, action);
    }
}

#[inline]
fn add_mapping<I: ApplicationInfo>(
    machine: &mut HelixMachine<TerminalKey, I>,
    modes: &MappedModes,
    keys: &str,
    action: &InputStep<I>,
) where
    I::Action: From<HelixCommand>,
{
    let (_, evs) = parse(keys).unwrap_or_else(|_| panic!("invalid helix keybinding: {keys}"));
    let modes = modes.split();

    for mode in modes {
        machine.add_mapping(mode, &evs, action);
    }
}

/// Add a key sequence to Space mode (`<Space>` then `keys`) that produces `actions`, for the
/// application's own commands.
pub fn add_space_mapping<I: ApplicationInfo>(
    machine: &mut HelixMachine<TerminalKey, I>,
    keys: &str,
    actions: Vec<Action<I>>,
) -> anyhow::Result<()>
where
    I::Action: From<HelixCommand>,
{
    let (_, evs) = parse(keys).map_err(|_| anyhow::anyhow!("invalid key sequence: {keys}"))?;
    let step = InputStep::new().actions(actions).goto(HelixMode::Normal);

    machine.add_mapping(HelixMode::Space, &evs, &step);

    Ok(())
}

/// A configurable collection of Helix bindings that can be added to a [ModalMachine].
#[derive(Debug)]
pub struct HelixBindings<I: ApplicationInfo> {
    prefixes: Vec<(MappedModes, &'static str, Option<InputStep<I>>)>,
    mappings: Vec<(MappedModes, &'static str, InputStep<I>)>,
    enter: Vec<(MappedModes, &'static str, InputStep<I>)>,
    modeless: Vec<(MappedModes, &'static str, InputStep<I>)>,
}

impl<I: ApplicationInfo> HelixBindings<I> {
    /// Leave `<Enter>` unmapped in every mode, for applications that decide what it does.
    pub fn without_enter(mut self) -> Self {
        self.enter = vec![];
        self
    }

    /// Run as a modeless text field: Insert mode only, shifted motions select.
    pub fn modeless(mut self) -> Self {
        self.modeless = modeless_keys();
        self
    }
}

impl<I: ApplicationInfo> Default for HelixBindings<I> {
    fn default() -> Self {
        HelixBindings {
            prefixes: default_pfxs(),
            mappings: default_keys(),
            enter: default_enter(),
            modeless: vec![],
        }
    }
}

impl<I: ApplicationInfo> InputBindings<TerminalKey, InputStep<I>> for HelixBindings<I>
where
    I::Action: From<HelixCommand>,
{
    fn setup(&self, machine: &mut HelixMachine<TerminalKey, I>) {
        for (modes, keys, action) in self.prefixes.iter() {
            add_prefix(machine, modes, keys, action);
        }

        for (modes, keys, action) in self.mappings.iter() {
            add_mapping(machine, modes, keys, action);
        }

        for (modes, keys, action) in self.enter.iter() {
            add_mapping(machine, modes, keys, action);
        }

        for (modes, keys, action) in self.modeless.iter() {
            add_mapping(machine, modes, keys, action);
        }
    }
}

/// Manage Helix keybindings and modes.
pub type HelixMachine<Key, T = EmptyInfo> = ModalMachine<Key, InputStep<T>>;

/// Create a new [HelixMachine] populated with Helix's default keys.
pub fn default_helix_keys<I: ApplicationInfo>() -> HelixMachine<TerminalKey, I>
where
    I::Action: From<HelixCommand>,
{
    ModalMachine::from_bindings::<HelixBindings<I>>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keybindings::BindingMachine;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(c: char) -> TerminalKey {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE).into()
    }

    #[test]
    fn test_select_mode_toggles() {
        let mut km: HelixMachine<TerminalKey> = default_helix_keys();
        assert_eq!(km.mode(), HelixMode::Normal);

        km.input_key(key('v'));
        assert_eq!(km.mode(), HelixMode::Select);

        km.input_key(key('v'));
        assert_eq!(km.mode(), HelixMode::Normal);
    }

    #[test]
    fn test_insert_and_back() {
        let mut km: HelixMachine<TerminalKey> = default_helix_keys();

        km.input_key(key('i'));
        assert_eq!(km.mode(), HelixMode::Insert);

        km.input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE).into());
        assert_eq!(km.mode(), HelixMode::Normal);
    }

    #[test]
    fn test_goto_mode_returns_to_normal() {
        let mut km: HelixMachine<TerminalKey> = default_helix_keys();

        km.input_key(key('g'));
        km.input_key(key('e'));
        assert_eq!(km.mode(), HelixMode::Normal);

        let (act, _) = km.pop().expect("ge moves");
        let exp = EditorAction::Edit(
            Specifier::Exact(EditAction::Motion),
            EditTarget::Boundary(RangeType::Buffer, true, MoveTerminus::End, Count::Contextual),
        );
        assert_eq!(act, exp.into());
    }
}
