//! Where a session ran, as the picker shows it: `~` for home, and when it does not fit, cut from
//! the left so the end of the path (the directory that tells sessions apart) is what stays.

use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;

use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

/// `path` with the home directory written `~`.
pub(super) fn shorten_home(path: &str) -> String {
    match dirs::home_dir() {
        Some(home) => match path.strip_prefix(home.to_string_lossy().as_ref()) {
            Some("") => "~".to_owned(),
            Some(rest) if rest.starts_with('/') => format!("~{rest}"),
            Some(_) | None => path.to_owned(),
        },
        None => path.to_owned(),
    }
}

/// Whether `path` is the directory Nori was started in: a path every session of the project
/// shares says nothing, so the picker leaves it out.
pub(super) fn is_here(path: &str) -> bool {
    static HERE: OnceLock<Option<PathBuf>> = OnceLock::new();
    HERE.get_or_init(|| std::env::current_dir().ok())
        .as_deref()
        .is_some_and(|here| here == Path::new(path))
}

/// `text` cut to `max` columns from the left, as `…/Worktrees/dictionary-recovered`: whole
/// path segments are kept when there is a segment boundary to cut at.
pub(super) fn clip_left(text: &str, max: usize) -> String {
    if UnicodeWidthStr::width(text) <= max {
        return text.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let mut width = 1;
    let mut start = text.len();
    for (index, ch) in text.char_indices().rev() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width > max {
            break;
        }
        width += ch_width;
        start = index;
    }
    let tail = &text[start..];
    let tail = match tail.find('/') {
        Some(slash) if slash > 0 => &tail[slash..],
        Some(_) | None => tail,
    };
    format!("…{tail}")
}
