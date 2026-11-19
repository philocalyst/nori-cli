//! How a stored thread is titled in the session list.

use unicode_segmentation::UnicodeSegmentation;

const SESSION_TITLE_MAX_GRAPHEMES: usize = 120;

fn truncate_graphemes(text: &str, max_graphemes: usize) -> String {
    let mut graphemes = text.grapheme_indices(true);

    if let Some((byte_index, _)) = graphemes.nth(max_graphemes) {
        if max_graphemes >= 3 {
            let mut truncate_graphemes = text.grapheme_indices(true);
            if let Some((truncate_byte_index, _)) = truncate_graphemes.nth(max_graphemes - 3) {
                let truncated = &text[..truncate_byte_index];
                format!("{truncated}...")
            } else {
                text.to_string()
            }
        } else {
            let truncated = &text[..byte_index];
            truncated.to_string()
        }
    } else {
        text.to_string()
    }
}

fn format_session_title(message: &str) -> Option<String> {
    let normalized = message.replace(['\r', '\n'], " ");
    let trimmed = normalized.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(truncate_graphemes(trimmed, SESSION_TITLE_MAX_GRAPHEMES))
    }
}

pub(super) fn stored_session_title(name: Option<&str>, preview: &str) -> Option<String> {
    [name, Some(preview)]
        .into_iter()
        .flatten()
        .find_map(format_session_title)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn stored_session_title_prefers_thread_name() {
        assert_eq!(
            stored_session_title(Some("renamed"), "preview"),
            Some("renamed".to_string())
        );
    }

    #[test]
    fn stored_session_title_falls_back_to_preview() {
        assert_eq!(
            stored_session_title(None, "preview"),
            Some("preview".to_string())
        );
        assert_eq!(
            stored_session_title(Some("  "), "preview"),
            Some("preview".to_string())
        );
    }
}
