//! The keymaps the editor can run.

use modalkit::actions::Action;
use modalkit::editing::key::KeyManager;
use modalkit::env::helix::keybindings::HelixBindings;
use modalkit::env::helix::keybindings::HelixMachine;
use modalkit::env::helix::keybindings::add_space_mapping;
use modalkit::env::vim::keybindings::default_vim_keys;
use modalkit::key::TerminalKey;
use modalkit::keybindings::ModalMachine;

use crate::editor::Keys;
use crate::info::EditorCommand;
use crate::info::EditorInfo;

/// Helix's space-menu keys that ask the host for something, and the id the host receives in
/// [`crate::EditorEvent::Host`]: `?` the command palette, `f` a file reference, `/` the history
/// search.
pub const SPACE_KEYS: &[(&str, &str)] = &[("?", "palette"), ("f", "files"), ("/", "search")];

/// Which keys edit the text.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum KeymapKind {
    /// Helix: selection first, modal (Normal, Select, Insert, and the `g`, `m`, `z` and space
    /// menus). The editor starts in Insert mode so typing works at once.
    #[default]
    Helix,
    /// Vim, modal.
    Vim,
    /// A modeless text field: typing inserts, shifted motions select, `Ctrl-Z` undoes, readline
    /// keys (`Ctrl-A`, `Ctrl-E`, `Ctrl-K`, `Ctrl-U`, `Ctrl-W`, `Alt-B`, `Alt-F`) move and delete.
    Standard,
}

impl KeymapKind {
    /// Whether the keymap has a Normal mode.
    pub fn is_modal(self) -> bool {
        match self {
            KeymapKind::Helix | KeymapKind::Vim => true,
            KeymapKind::Standard => false,
        }
    }

    pub(crate) fn machine(self) -> Keys {
        match self {
            KeymapKind::Helix => {
                let mut machine: HelixMachine<TerminalKey, EditorInfo> =
                    ModalMachine::from_bindings::<HelixBindings<EditorInfo>>();
                // The space menu's application keys, as Helix lays them out.
                for (keys, host) in SPACE_KEYS {
                    let action = Action::Application(EditorCommand::Host((*host).to_owned()));
                    // The keys are fixed and valid: a failure here is a typo caught by the tests.
                    let _ = add_space_mapping(&mut machine, keys, vec![action]);
                }
                KeyManager::new(machine)
            }
            KeymapKind::Standard => {
                let mut machine: HelixMachine<TerminalKey, EditorInfo> = ModalMachine::empty();
                modalkit::keybindings::InputBindings::setup(
                    &HelixBindings::<EditorInfo>::default().modeless(),
                    &mut machine,
                );
                KeyManager::new(machine)
            }
            KeymapKind::Vim => KeyManager::new(default_vim_keys::<EditorInfo>()),
        }
    }
}
