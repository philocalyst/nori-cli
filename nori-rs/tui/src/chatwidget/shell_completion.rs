//! Tab completion of the shell command in the composer (`!` mode), from the in-process shell.
//!
//! The shell's own completion engine answers (programmable `complete` specs, commands,
//! functions, variables, paths). One candidate is inserted; several extend the word to the
//! longest prefix they share, and when that adds nothing the candidates are listed in a toast.

use nori_bash::CompletionCandidate;

use super::ChatWidget;
use crate::app_event::AppEvent;

/// How many candidates a toast lists.
const LISTED: usize = 8;

impl ChatWidget {
    /// Ask the shell to complete `text` at byte `cursor`.
    pub(crate) fn request_shell_completion(&mut self, text: String, cursor: usize) {
        let session = self.user_shell_state.local_session();
        let cwd = self.config.cwd.clone();
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let mut guard = session.lock().await;
            if guard.is_none()
                && let Ok(shell) = nori_bash::BashSession::new(&cwd).await
            {
                *guard = Some(shell);
            }
            let Some(shell) = guard.as_mut() else {
                return;
            };
            let candidates = shell
                .complete(&text, i64::try_from(cursor).unwrap_or(0))
                .await;
            tx.send(AppEvent::ShellCompleted { text, candidates });
        });
    }

    /// Apply the shell's answer, unless the composer moved on while it was being worked out.
    pub(crate) fn apply_shell_completion(
        &mut self,
        text: &str,
        candidates: &[CompletionCandidate],
    ) {
        if !self.bottom_pane.shell_text_is(text) || candidates.is_empty() {
            return;
        }
        let (start, len) = (
            usize::try_from(candidates[0].replace_start).unwrap_or(0),
            usize::try_from(candidates[0].replace_len).unwrap_or(0),
        );
        let typed = text.get(start..start + len).unwrap_or("");
        let common = common_prefix(candidates.iter().map(|candidate| candidate.text.as_str()));
        let only = candidates.len() == 1;
        if only || common.len() > typed.len() {
            let mut insert = common;
            if only && !candidates[0].text.ends_with('/') {
                insert.push(' ');
            }
            self.bottom_pane
                .replace_shell_range(start..start + len, &insert);
            return;
        }
        let names: Vec<&str> = candidates
            .iter()
            .take(LISTED)
            .map(|candidate| candidate.text.as_str())
            .collect();
        let more = candidates.len().saturating_sub(LISTED);
        let mut list = names.join("  ");
        if more > 0 {
            list.push_str(&format!("  +{more}"));
        }
        self.app_event_tx.send(AppEvent::TransientNotice(list));
    }
}

fn common_prefix<'a>(mut words: impl Iterator<Item = &'a str>) -> String {
    let Some(first) = words.next() else {
        return String::new();
    };
    let mut prefix = first.to_owned();
    for word in words {
        let shared = prefix
            .chars()
            .zip(word.chars())
            .take_while(|(a, b)| a == b)
            .map(|(a, _)| a.len_utf8())
            .sum();
        prefix.truncate(shared);
    }
    prefix
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn the_prefix_is_what_all_words_share() {
        assert_eq!(
            common_prefix(["cargo", "cargo-fmt", "cargo-nb"].into_iter()),
            "cargo"
        );
        assert_eq!(common_prefix(["a", "b"].into_iter()), "");
        assert_eq!(common_prefix(std::iter::empty()), "");
    }
}
