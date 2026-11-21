//! Keys into the editor, and what the editor asks of the composer in return: the system
//! clipboard (`space y`, `space p`), the command palette (`:`), language-server jumps, notices.

use super::*;
use nori_editor::ClipboardCommand;
use nori_editor::EditorEvent;

impl ChatComposer {
    /// Give `key` to the editor and carry out what it asks for.
    pub(super) fn editor_input(&mut self, key: KeyEvent) {
        let before = self.textarea.mode();
        self.textarea.input(key);
        if before == EditorMode::Insert && self.textarea.mode() != EditorMode::Insert {
            self.left_insert_at = Some(Instant::now());
        }
        for event in self.textarea.take_events() {
            self.editor_event(event);
        }
    }

    fn editor_event(&mut self, event: EditorEvent) {
        match event {
            EditorEvent::CommandPalette => self.open_palette(),
            EditorEvent::Lsp(request, _) => {
                self.app_event_tx.send(AppEvent::TransientNotice(format!(
                    "No language server answers {request:?} in the prompt yet."
                )));
            }
            EditorEvent::CopyToClipboard(text) => {
                let tx = self.app_event_tx.clone();
                // The clipboard can stall (X11, remote sessions): never on the UI thread.
                std::thread::spawn(move || {
                    if let Err(error) = copy_to_clipboard(&text) {
                        tx.send(AppEvent::TransientNotice(format!(
                            "Could not copy: {error}"
                        )));
                    }
                });
            }
            EditorEvent::PasteFromClipboard(command) => {
                let tx = self.app_event_tx.clone();
                std::thread::spawn(move || match read_clipboard() {
                    Ok(text) => tx.send(AppEvent::EditorClipboard { command, text }),
                    Err(error) => {
                        tx.send(AppEvent::TransientNotice(format!(
                            "Could not paste: {error}"
                        )));
                    }
                });
            }
            EditorEvent::Host(id) => match id.as_str() {
                "palette" => self.open_palette(),
                "files" => {
                    // An `@` token at the cursor opens the file search popup.
                    self.textarea.enter_insert();
                    let at = self.textarea.cursor();
                    let needs_space = self
                        .textarea
                        .text()
                        .get(..at)
                        .and_then(|before| before.chars().next_back())
                        .is_some_and(|ch| !ch.is_whitespace());
                    self.textarea
                        .insert_str(if needs_space { " @" } else { "@" });
                }
                "search" => {
                    self.active_popup = ActivePopup::HistorySearch(HistorySearchPopup::new());
                    self.app_event_tx.send(AppEvent::HarnessAction(
                        crate::app_event::HarnessAction::SearchHistory { max_results: 500 },
                    ));
                }
                _ => {}
            },
            EditorEvent::Notice(text) => {
                self.app_event_tx.send(AppEvent::TransientNotice(text));
            }
        }
    }

    /// The clipboard's text, read for an earlier `space p` / `space P` / `space R`.
    pub(crate) fn paste_clipboard(&mut self, command: ClipboardCommand, text: &str) {
        if !self.input_enabled {
            return;
        }
        self.textarea.editor_mut().paste_clipboard(command, text);
        self.sync_selection_popups();
    }
}

#[cfg(not(target_os = "android"))]
fn copy_to_clipboard(text: &str) -> Result<(), String> {
    arboard::Clipboard::new()
        .and_then(|mut clipboard| clipboard.set_text(text.to_owned()))
        .map_err(|error| error.to_string())
}

#[cfg(not(target_os = "android"))]
fn read_clipboard() -> Result<String, String> {
    arboard::Clipboard::new()
        .and_then(|mut clipboard| clipboard.get_text())
        .map_err(|error| error.to_string())
}

#[cfg(target_os = "android")]
fn copy_to_clipboard(_text: &str) -> Result<(), String> {
    Err("there is no clipboard here".to_owned())
}

#[cfg(target_os = "android")]
fn read_clipboard() -> Result<String, String> {
    Err("there is no clipboard here".to_owned())
}
