//! The keymap: every key the TUI itself answers (as opposed to the text editor's own keys), in
//! one table.
//!
//! An entry is a *mode* (where the key applies), an *action* (what it does), the bindings that
//! trigger it, and a line of help. The TUI looks keys up here to act, and builds its `?` list
//! from the same entries, so behaviour, overlay and docs read from one place and cannot drift.
//! A user rebinds an entry under `[tui.keymap]` by its id:
//!
//! ```toml
//! [tui.keymap]
//! "focus.rewind" = ["r", "ctrl+r"]
//! "global.toggle_tool" = "alt+t"
//! ```
//!
//! Entries that are the text editor's or the composer's typing (`!`, `/`, `@`, `?`) are *fixed*:
//! listed so the overlay is complete, not rebindable. Entries that stand for an existing
//! `[tui.hotkeys]` binding (clear screen, transcript, editor, actor panel) follow that binding.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::types::HotkeyAction;
use crate::types::HotkeyBinding;
use crate::types::HotkeyConfig;

/// Where a key applies.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyMode {
    /// In every state the TUI is in, except where a popup or overlay swallows keys.
    Global,
    /// While a prompt is focused for rewinding.
    Focus,
    /// In the composer, when idle.
    Composer,
}

impl KeyMode {
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Focus => "focus",
            Self::Composer => "composer",
        }
    }
}

/// What a key does.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyAction {
    ClearScreen,
    CycleSubagent,
    CycleSubagentBack,
    ParentSession,
    ToggleTool,
    ScrollChildUp,
    ScrollChildDown,
    FocusOlder,
    FocusNewer,
    FocusEdit,
    FocusRewind,
    FocusRewindKeepFiles,
    FocusFork,
    FocusToggleTool,
    FocusLeave,
    /// `Esc` on an empty composer while idle.
    EnterFocus,
    /// `[` in normal mode.
    EnterFocusBracket,
    OpenTranscript,
    OpenEditor,
    OpenActorPanel,
    /// Open the action palette (works mid-draft; the draft is kept).
    Palette,
    /// `↑` at the top of the composer: walk back to earlier prompts; fixed.
    WalkBack,
    /// Typing that means something at the start of the composer; fixed.
    ShowKeys,
    ShellMode,
    SlashCommands,
    FilePaths,
}

/// Where an entry's bindings come from.
#[derive(Clone, Debug)]
enum Source {
    Keys(Vec<HotkeyBinding>),
    Hotkey(HotkeyAction),
    /// Fixed: shown in the help, not rebindable.
    Fixed(&'static str),
}

#[derive(Clone, Debug)]
pub struct KeyEntry {
    pub mode: KeyMode,
    pub action: KeyAction,
    /// The id a user rebinds it by, `mode.name`.
    pub id: &'static str,
    pub help: &'static str,
    /// Whether the `?` list shows it.
    pub listed: bool,
    source: Source,
}

/// One row of the `?` list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HelpRow {
    pub keys: String,
    pub what: &'static str,
}

#[derive(Clone, Debug)]
pub struct Keymap {
    entries: Vec<KeyEntry>,
}

/// What `[tui.keymap]` may hold for one entry: a key, or several.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum KeyOverride {
    One(String),
    Many(Vec<String>),
}

fn keys(list: &[&str]) -> Source {
    Source::Keys(
        list.iter()
            .map(|key| HotkeyBinding::from_str(key))
            .collect(),
    )
}

fn entry(
    mode: KeyMode,
    action: KeyAction,
    id: &'static str,
    source: Source,
    help: &'static str,
    listed: bool,
) -> KeyEntry {
    KeyEntry {
        mode,
        action,
        id,
        help,
        listed,
        source,
    }
}

impl Default for Keymap {
    fn default() -> Self {
        use KeyAction as A;
        use KeyMode::Composer;
        use KeyMode::Focus;
        use KeyMode::Global;
        Self {
            entries: vec![
                entry(
                    Composer,
                    A::Palette,
                    "composer.palette",
                    keys(&["alt+x"]),
                    "actions: model, effort, sessions, settings… (keeps the draft)",
                    true,
                ),
                entry(
                    Composer,
                    A::WalkBack,
                    "composer.walk_back",
                    Source::Fixed("↑"),
                    "at the top of the draft: earlier prompts; Enter rewinds there",
                    true,
                ),
                entry(
                    Composer,
                    A::EnterFocus,
                    "composer.focus_back",
                    keys(&["esc"]),
                    "interrupt a turn, or stop running subagents",
                    true,
                ),
                entry(
                    Composer,
                    A::EnterFocusBracket,
                    "composer.focus_back_bracket",
                    keys(&["["]),
                    "step back to the last prompt (normal mode)",
                    false,
                ),
                entry(
                    Focus,
                    A::FocusOlder,
                    "focus.older",
                    keys(&["up", "k", "["]),
                    "previous prompt",
                    true,
                ),
                entry(
                    Focus,
                    A::FocusNewer,
                    "focus.newer",
                    keys(&["down", "j", "]"]),
                    "next prompt; past the last one, leave",
                    true,
                ),
                entry(
                    Focus,
                    A::FocusEdit,
                    "focus.edit",
                    keys(&["enter", "e", "i"]),
                    "edit the prompt in place; Enter again sends it",
                    true,
                ),
                entry(
                    Focus,
                    A::FocusRewind,
                    "focus.rewind",
                    keys(&["r"]),
                    "rewind to it, restoring files",
                    true,
                ),
                entry(
                    Focus,
                    A::FocusRewindKeepFiles,
                    "focus.rewind_keep_files",
                    keys(&["shift+r"]),
                    "rewind to it, keeping files",
                    true,
                ),
                entry(
                    Focus,
                    A::FocusFork,
                    "focus.fork",
                    keys(&["f", "space"]),
                    "fork a subagent from its response",
                    true,
                ),
                entry(
                    Focus,
                    A::FocusToggleTool,
                    "focus.toggle_tool",
                    keys(&["t"]),
                    "expand the nearest card, and leave",
                    false,
                ),
                entry(
                    Focus,
                    A::FocusLeave,
                    "focus.leave",
                    keys(&["esc"]),
                    "leave",
                    false,
                ),
                entry(
                    Global,
                    A::ClearScreen,
                    "global.clear_screen",
                    Source::Hotkey(HotkeyAction::Redraw),
                    "push the screen into the scrollback",
                    true,
                ),
                entry(
                    Global,
                    A::ToggleTool,
                    "global.toggle_tool",
                    keys(&["alt+e"]),
                    "expand or collapse a tool card",
                    true,
                ),
                entry(
                    Global,
                    A::CycleSubagent,
                    "global.next_actor",
                    keys(&["ctrl+tab"]),
                    "next actor",
                    false,
                ),
                entry(
                    Global,
                    A::CycleSubagentBack,
                    "global.previous_actor",
                    keys(&["ctrl+shift+tab"]),
                    "previous actor",
                    false,
                ),
                entry(
                    Global,
                    A::ParentSession,
                    "global.parent",
                    keys(&["tab"]),
                    "from an actor's view, back to its parent",
                    false,
                ),
                entry(
                    Global,
                    A::ScrollChildUp,
                    "global.scroll_up",
                    keys(&["pageup"]),
                    "scroll an actor's view",
                    false,
                ),
                entry(
                    Global,
                    A::ScrollChildDown,
                    "global.scroll_down",
                    keys(&["pagedown"]),
                    "scroll an actor's view",
                    false,
                ),
                entry(
                    Global,
                    A::OpenActorPanel,
                    "global.actors",
                    Source::Hotkey(HotkeyAction::SwitchSubagent),
                    "the actors panel",
                    true,
                ),
                entry(
                    Global,
                    A::OpenTranscript,
                    "global.transcript",
                    Source::Hotkey(HotkeyAction::OpenTranscript),
                    "the transcript",
                    true,
                ),
                entry(
                    Global,
                    A::OpenEditor,
                    "global.editor",
                    Source::Hotkey(HotkeyAction::OpenEditor),
                    "edit the prompt in your editor",
                    true,
                ),
                entry(
                    Composer,
                    A::ShowKeys,
                    "composer.keys",
                    Source::Fixed("?"),
                    "this list",
                    true,
                ),
                entry(
                    Composer,
                    A::ShellMode,
                    "composer.shell",
                    Source::Fixed("!"),
                    "run a shell command",
                    true,
                ),
                entry(
                    Composer,
                    A::SlashCommands,
                    "composer.commands",
                    Source::Fixed("/"),
                    "commands",
                    true,
                ),
                entry(
                    Composer,
                    A::FilePaths,
                    "composer.files",
                    Source::Fixed("@"),
                    "file paths",
                    true,
                ),
            ],
        }
    }
}

impl Keymap {
    /// The default keymap with the user's `[tui.keymap]` overrides applied. Ids that name no
    /// entry, fixed entries and unparsable values are reported and left alone.
    pub fn with_overrides(overrides: &BTreeMap<String, KeyOverride>) -> (Self, Vec<String>) {
        let mut keymap = Self::default();
        let mut problems = Vec::new();
        for (id, value) in overrides {
            let Some(found) = keymap.entries.iter_mut().find(|entry| entry.id == id) else {
                problems.push(format!("[tui.keymap] has no entry `{id}`"));
                continue;
            };
            if matches!(found.source, Source::Fixed(_)) {
                problems.push(format!(
                    "[tui.keymap] `{id}` is fixed and cannot be rebound"
                ));
                continue;
            }
            let list: Vec<&str> = match value {
                KeyOverride::One(key) => vec![key.as_str()],
                KeyOverride::Many(list) => list.iter().map(String::as_str).collect(),
            };
            found.source = keys(&list);
        }
        (keymap, problems)
    }

    pub fn entries(&self) -> &[KeyEntry] {
        &self.entries
    }

    /// The bindings that trigger `entry` now.
    pub fn bindings(&self, entry: &KeyEntry, hotkeys: &HotkeyConfig) -> Vec<HotkeyBinding> {
        match &entry.source {
            Source::Keys(list) => list.clone(),
            Source::Hotkey(action) => vec![hotkeys.binding_for(*action).clone()],
            Source::Fixed(_) => Vec::new(),
        }
    }

    /// The entries of `mode` whose bindings satisfy `matches`, in table order. The first one is
    /// what a key does.
    pub fn find(
        &self,
        mode: KeyMode,
        hotkeys: &HotkeyConfig,
        mut matches: impl FnMut(&HotkeyBinding) -> bool,
    ) -> Option<KeyAction> {
        self.entries
            .iter()
            .filter(|entry| entry.mode == mode)
            .find(|entry| self.bindings(entry, hotkeys).iter().any(&mut matches))
            .map(|entry| entry.action)
    }

    /// The rows of the `?` list: what each listed entry is bound to now, and what it does.
    pub fn help(&self, hotkeys: &HotkeyConfig, use_shift_enter: bool) -> Vec<HelpRow> {
        let mut rows: Vec<HelpRow> = self
            .entries
            .iter()
            .filter(|entry| entry.listed)
            .map(|entry| HelpRow {
                keys: match &entry.source {
                    Source::Fixed(key) => (*key).to_owned(),
                    _ => self
                        .bindings(entry, hotkeys)
                        .iter()
                        .filter(|binding| !binding.is_none())
                        .map(|binding| label(binding.as_str()))
                        .collect::<Vec<_>>()
                        .join("  "),
                },
                what: entry.help,
            })
            .filter(|row| !row.keys.is_empty())
            .collect();
        rows.push(HelpRow {
            keys: if use_shift_enter {
                "Shift+Enter"
            } else {
                "Alt+Enter"
            }
            .to_owned(),
            what: "newline",
        });
        rows
    }
}

/// `ctrl+l` as `Ctrl+L`, `esc` as `Esc`, `[` as `[`.
fn label(binding: &str) -> String {
    binding
        .split('+')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) if part.chars().count() > 1 => {
                    first.to_uppercase().chain(chars).collect::<String>()
                }
                Some(first) => first.to_uppercase().to_string(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn overrides_replace_bindings_and_report_what_they_cannot_apply() {
        let overrides = BTreeMap::from([
            (
                "focus.rewind".to_owned(),
                KeyOverride::One("ctrl+r".to_owned()),
            ),
            (
                "composer.shell".to_owned(),
                KeyOverride::One("x".to_owned()),
            ),
            ("nope".to_owned(), KeyOverride::One("x".to_owned())),
        ]);
        let (keymap, problems) = Keymap::with_overrides(&overrides);
        assert_eq!(problems.len(), 2);
        let hotkeys = HotkeyConfig::default();
        let rewind = keymap
            .entries()
            .iter()
            .find(|entry| entry.id == "focus.rewind");
        assert_eq!(
            keymap
                .bindings(rewind.expect("entry"), &hotkeys)
                .iter()
                .map(HotkeyBinding::as_str)
                .collect::<Vec<_>>(),
            vec!["ctrl+r"]
        );
    }

    #[test]
    fn the_help_follows_the_bindings() {
        let (keymap, _) = Keymap::with_overrides(&BTreeMap::from([(
            "global.toggle_tool".to_owned(),
            KeyOverride::One("alt+t".to_owned()),
        )]));
        let rows = keymap.help(&HotkeyConfig::default(), false);
        assert!(
            rows.iter()
                .any(|row| row.keys == "Alt+T" && row.what.contains("tool card"))
        );
        assert!(rows.iter().any(|row| row.keys == "Ctrl+L"));
    }
}
