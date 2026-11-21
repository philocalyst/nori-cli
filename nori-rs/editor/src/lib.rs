//! Nori's text editor: the one component every editable text in the TUI is made of (the composer,
//! and with it every past prompt walked back to).
//!
//! It is built on modalkit (vendored at `../vendor/modalkit`): modalkit's buffer does the editing
//! (multiple selections, registers, undo history, text objects, search, macros, repeat) and its
//! keymap machines turn keys into editing actions. Three keymaps:
//!
//! - [KeymapKind::Helix], the default: Helix's selection-first modal keys, from the Helix
//!   environment added to the vendored modalkit;
//! - [KeymapKind::Vim];
//! - [KeymapKind::Standard], a modeless text field with readline keys.
//!
//! What it draws is a pure function of the text and the width ([Layout]): typing, pasting,
//! moving and resizing never move text that is not being edited, and the only drawing state is
//! the first row shown, which moves just far enough to keep the cursor visible.
//!
//! Chips ([chips]) are atomic tokens in the text (pastes, attachments, references, directives),
//! one character each in the buffer, drawn as labels.

pub mod chips;
mod editor;
mod info;
mod keymap;
mod keys;
mod layout;
mod lines;
mod prompt;
mod render;
mod surround;
mod text;
mod words;

pub use editor::Change;
pub use editor::Edge;
pub use editor::Editor;
pub use editor::EditorEvent;
pub use editor::Selection;
pub use info::EditorCommand;
pub use keymap::KeymapKind;
pub use keys::CursorShape;
pub use keys::EditorMode;
pub use keys::Menu;
pub use layout::Layout;
pub use layout::Row;
pub use modalkit::env::helix::ClipboardCommand;
pub use modalkit::env::helix::LspCommand;
pub use render::EditorStyle;
pub use render::Highlight;

#[cfg(test)]
mod tests;
