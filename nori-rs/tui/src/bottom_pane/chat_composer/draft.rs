//! Transfer an in-progress edit without flattening it into replacement text.

use super::*;

/// Owned editing state for a widget handoff. Moving the textarea preserves its
/// cursor, atomic placeholders, undo history, and Vim editing state. Session
/// metadata, agent commands, and popup results stay with their owning widget.
#[must_use]
pub(crate) struct ComposerDraft {
    textarea: TextArea,
    pending_pastes: Vec<(ChipId, String)>,
    attached_images: Vec<AttachedImage>,
    is_shell_mode: bool,
    paste_burst: PasteBurst,
}

impl ChatComposer {
    /// Take the edit before discarding this composer. A buffered first character
    /// or paste is part of the draft even when `current_text()` is still empty.
    pub(crate) fn take_draft(&mut self) -> ComposerDraft {
        let mut fresh = TextArea::new();
        fresh.set_hotkey_config(self.textarea.hotkey_config().clone());
        fresh.set_keymap(self.textarea.keymap());
        ComposerDraft {
            textarea: std::mem::replace(&mut self.textarea, fresh),
            pending_pastes: std::mem::take(&mut self.pending_pastes),
            attached_images: std::mem::take(&mut self.attached_images),
            is_shell_mode: std::mem::take(&mut self.is_shell_mode),
            paste_burst: std::mem::take(&mut self.paste_burst),
        }
    }

    /// Restore an edit into a replacement widget, including the unflushed input
    /// and its original deadline. The next paste tick continues the same burst.
    pub(crate) fn restore_draft(&mut self, draft: ComposerDraft) {
        self.textarea = draft.textarea;
        self.pending_pastes = draft.pending_pastes;
        self.attached_images = draft.attached_images;
        self.is_shell_mode = draft.is_shell_mode;
        self.paste_burst = draft.paste_burst;
        self.sync_selection_popups();
    }
}
