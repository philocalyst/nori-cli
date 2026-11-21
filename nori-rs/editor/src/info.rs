//! The editor's application types for modalkit: what its keymaps may ask of it beyond editing.

use modalkit::editing::application::ApplicationAction;
use modalkit::editing::application::ApplicationInfo;
use modalkit::editing::context::EditContext;
use modalkit::env::helix::HelixCommand;
use modalkit::keybindings::SequenceStatus;
use modalkit::prelude::CommandType;

/// What a key asks of the editor that is not an edit of its text: Helix's extras, and the
/// commands an application maps into the editor's keymap (Helix's space menu).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EditorCommand {
    Helix(HelixCommand),
    /// An application command bound into the keymap, by its id.
    Host(String),
}

impl From<HelixCommand> for EditorCommand {
    fn from(command: HelixCommand) -> Self {
        EditorCommand::Helix(command)
    }
}

impl ApplicationAction for EditorCommand {
    fn is_edit_sequence(&self, _: &EditContext) -> SequenceStatus {
        match self {
            EditorCommand::Helix(
                HelixCommand::SurroundAdd(_)
                | HelixCommand::SurroundReplace(..)
                | HelixCommand::SurroundDelete(_)
                | HelixCommand::Indent(_),
            ) => SequenceStatus::Track,
            EditorCommand::Helix(HelixCommand::Word(..) | HelixCommand::SelectLines(_)) => {
                SequenceStatus::Ignore
            }
            EditorCommand::Helix(
                HelixCommand::SearchSelection
                | HelixCommand::CommandPalette
                | HelixCommand::Lsp(_)
                | HelixCommand::Clipboard(_),
            )
            | EditorCommand::Host(_) => SequenceStatus::Break,
        }
    }

    fn is_last_action(&self, _: &EditContext) -> SequenceStatus {
        SequenceStatus::Ignore
    }

    fn is_last_selection(&self, _: &EditContext) -> SequenceStatus {
        SequenceStatus::Ignore
    }

    fn is_switchable(&self, _: &EditContext) -> bool {
        false
    }
}

/// The editor's [ApplicationInfo]: one buffer, named by a string.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EditorInfo {}

impl ApplicationInfo for EditorInfo {
    type Error = String;
    type Action = EditorCommand;
    type Store = ();
    type WindowId = String;
    type ContentId = String;

    fn content_of_command(cmdtype: CommandType) -> String {
        match cmdtype {
            CommandType::Application => "*application*".into(),
            CommandType::Command => "*command*".into(),
            CommandType::Content => "*content*".into(),
            CommandType::Search => "*search*".into(),
            CommandType::Shell => "*shell*".into(),
        }
    }
}
