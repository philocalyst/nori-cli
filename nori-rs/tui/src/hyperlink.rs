//! Terminal hyperlinks (OSC 8) for what the conversation shows.
//!
//! The conversation is the terminal's own text, so its links must be the terminal's too: they work
//! in the scrollback, after the program exits, with the terminal's own click and hover, and
//! without Nori capturing the mouse. Nothing upstream has to mark a link: both writers (the frame
//! diff in `custom_terminal` and the rows typed into the scrollback in `stage::write`) find them in
//! the text they write. A link is a URL (`http`, `https`, `file`), or a path that names a file or
//! directory that exists, relative to the working directory or absolute, with an optional
//! `:line[:column]` that stays part of the visible link but not of its target.
//!
//! A URL cut across two rows by wrapping is one link: a row whose URL runs to its last column
//! continues into the first word of the next row, and both halves carry the whole target.

use std::collections::HashMap;
use std::ops::Range;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;

/// One row of text as written: its characters, and the column each byte of them starts at.
pub(crate) struct RowText {
    text: String,
    /// The column of every byte of `text` (continuation bytes repeat their character's column).
    columns: Vec<u16>,
    /// The columns the row has.
    width: u16,
}

impl RowText {
    pub(crate) fn new(width: u16) -> Self {
        Self {
            text: String::new(),
            columns: Vec::new(),
            width,
        }
    }

    /// Append the symbol drawn at `column`.
    pub(crate) fn push(&mut self, column: u16, symbol: &str) {
        self.text.push_str(symbol);
        self.columns
            .extend(std::iter::repeat_n(column, symbol.len()));
    }

    fn column(&self, byte: usize) -> u16 {
        self.columns
            .get(byte)
            .copied()
            .unwrap_or_else(|| self.columns.last().map_or(0, |column| column + 1))
    }

    /// The columns of `bytes`.
    fn span(&self, bytes: Range<usize>) -> Range<u16> {
        let start = self.column(bytes.start);
        let end = if bytes.end >= self.text.len() {
            self.columns.last().map_or(start, |column| column + 1)
        } else {
            self.column(bytes.end)
        };
        start..end.max(start + 1)
    }

    /// Whether `bytes` reaches the last column the row has text in, at its right edge.
    fn reaches_edge(&self, bytes: &Range<usize>) -> bool {
        let rest = &self.text[bytes.end..];
        rest.chars().all(char::is_whitespace)
            && self.column(bytes.end.saturating_sub(1)) + 1 >= self.width
    }
}

/// A link on a row: the columns it covers and what it opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Link {
    pub(crate) columns: Range<u16>,
    pub(crate) target: String,
}

/// The links of `row`, given the rows above and below it so a URL wrapped across rows is whole.
pub(crate) fn links(
    previous: Option<&RowText>,
    row: &RowText,
    next: Option<&RowText>,
) -> Vec<Link> {
    if !enabled() {
        return Vec::new();
    }
    let mut found = Vec::new();
    // The head of a URL the row above did not finish.
    let mut skip = 0;
    if let Some(previous) = previous
        && let Some(url) = scan_urls(&previous.text)
            .into_iter()
            .rev()
            .find(|url| previous.reaches_edge(url))
        && let Some(tail) = continuation(&row.text)
    {
        let target = format!("{}{}", &previous.text[url], &row.text[tail.clone()]);
        found.push(Link {
            columns: row.span(tail.clone()),
            target,
        });
        skip = tail.end;
    }
    for url in scan_urls(&row.text) {
        if url.start < skip {
            continue;
        }
        let mut target = row.text[url.clone()].to_owned();
        if row.reaches_edge(&url)
            && let Some(next) = next
            && let Some(tail) = continuation(&next.text)
        {
            target.push_str(&next.text[tail]);
        }
        found.push(Link {
            columns: row.span(url),
            target,
        });
    }
    for (bytes, target) in scan_paths(&row.text) {
        if bytes.start < skip
            || found
                .iter()
                .any(|link| ranges_overlap(&link.columns, &row.span(bytes.clone())))
        {
            continue;
        }
        found.push(Link {
            columns: row.span(bytes),
            target,
        });
    }
    found.sort_by_key(|link| link.columns.start);
    found
}

thread_local! {
    /// Links a widget placed on the frame being drawn: text that is not itself a URL (a short
    /// "open the sign-in page"), or a URL shown cut short. Cleared when a frame begins.
    static PLACED: std::cell::RefCell<Vec<(ratatui::layout::Rect, String)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Make the cells of `area` (screen coordinates) a link to `target` in the frame being drawn.
pub(crate) fn place(area: ratatui::layout::Rect, target: impl Into<String>) {
    let target = target.into();
    PLACED.with(|placed| placed.borrow_mut().push((area, target)));
}

/// A frame begins: the links placed for the last one are gone.
pub(crate) fn begin_frame() {
    PLACED.with(|placed| placed.borrow_mut().clear());
}

/// The links of row `y` of a frame's cell grid, seeing the rows around it for wrapped URLs. A
/// link a widget placed wins over one found in the text under it.
pub(crate) fn buffer_row_links(buffer: &ratatui::buffer::Buffer, y: u16) -> Vec<Link> {
    if !enabled() {
        return Vec::new();
    }
    let row = buffer_row_text(buffer, y);
    let previous = y.checked_sub(1).map(|y| buffer_row_text(buffer, y));
    let next = (y + 1 < buffer.area.height).then(|| buffer_row_text(buffer, y + 1));
    let mut found = links(previous.as_ref(), &row, next.as_ref());
    let screen_y = buffer.area.y + y;
    PLACED.with(|placed| {
        for (area, target) in placed.borrow().iter() {
            if !(area.top()..area.bottom()).contains(&screen_y) {
                continue;
            }
            let columns = area.left().saturating_sub(buffer.area.x)
                ..area.right().saturating_sub(buffer.area.x);
            found.retain(|link| !ranges_overlap(&link.columns, &columns));
            found.push(Link {
                columns,
                target: target.clone(),
            });
        }
    });
    found.sort_by_key(|link| link.columns.start);
    found
}

/// Row `y` of `buffer` as text: a wide glyph is read once, not once per column it covers.
fn buffer_row_text(buffer: &ratatui::buffer::Buffer, y: u16) -> RowText {
    use unicode_width::UnicodeWidthStr;
    let width = buffer.area.width;
    let mut text = RowText::new(width);
    let start = usize::from(y) * usize::from(width);
    let mut skip = 0usize;
    for (column, cell) in buffer.content[start..start + usize::from(width)]
        .iter()
        .enumerate()
    {
        if skip > 0 {
            skip -= 1;
            continue;
        }
        let symbol = cell.symbol();
        skip = symbol.width().saturating_sub(1);
        text.push(u16::try_from(column).unwrap_or(u16::MAX), symbol);
    }
    text
}

/// The links of `rows`, rows of text typed one under another, each seeing its neighbours.
pub(crate) fn line_links(rows: &[RowText], index: usize) -> Vec<Link> {
    links(
        index.checked_sub(1).and_then(|previous| rows.get(previous)),
        &rows[index],
        rows.get(index + 1),
    )
}

/// The OSC 8 sequence that starts a link to `target` (or ends the current one, for `None`).
pub(crate) fn osc8(target: Option<&str>) -> String {
    match target {
        Some(target) => {
            // One id per target, so a terminal highlights every row of a wrapped link together.
            let id = target
                .bytes()
                .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
                    (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
                });
            format!("\x1b]8;id=n{id:x};{target}\x1b\\")
        }
        None => "\x1b]8;;\x1b\\".to_owned(),
    }
}

/// Links are written unless the terminal is known not to want them (`NORI_NO_HYPERLINKS`, a dumb
/// terminal). Terminals without OSC 8 support ignore the sequence.
fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var_os("NORI_NO_HYPERLINKS").is_none()
            && std::env::var("TERM").map_or(true, |term| term != "dumb")
    })
}

fn ranges_overlap(a: &Range<u16>, b: &Range<u16>) -> bool {
    a.start < b.end && b.start < a.end
}

const SCHEMES: [&str; 3] = ["https://", "http://", "file://"];

/// The URLs written in `text`, in order.
pub(crate) fn urls_in(text: &str) -> Vec<String> {
    scan_urls(text)
        .into_iter()
        .map(|range| text[range].to_owned())
        .collect()
}

/// The byte ranges of the URLs in `text`.
fn scan_urls(text: &str) -> Vec<Range<usize>> {
    let mut urls = Vec::new();
    let mut from = 0;
    while let Some((start, _)) = SCHEMES
        .iter()
        .filter_map(|scheme| text[from..].find(scheme).map(|at| (from + at, scheme)))
        .min_by_key(|(at, _)| *at)
    {
        let end = start
            + text[start..]
                .find(|c: char| !is_url_char(c))
                .unwrap_or(text.len() - start);
        let end = trim_url_end(&text[start..end]) + start;
        if end > start + 8 {
            urls.push(start..end);
        }
        from = end.max(start + 1);
    }
    urls
}

fn is_url_char(c: char) -> bool {
    c.is_ascii_graphic()
        && !matches!(
            c,
            '<' | '>' | '"' | '\'' | '`' | '{' | '}' | '|' | '\\' | '^'
        )
}

/// Where a URL candidate really ends: trailing punctuation and unbalanced closing brackets belong
/// to the sentence around it.
fn trim_url_end(url: &str) -> usize {
    let mut end = url.len();
    loop {
        let Some(last) = url[..end].chars().last() else {
            return end;
        };
        let unbalanced = |open: char, close: char| {
            last == close && url[..end].matches(close).count() > url[..end].matches(open).count()
        };
        if matches!(last, '.' | ',' | ';' | ':' | '!' | '?')
            || unbalanced('(', ')')
            || unbalanced('[', ']')
        {
            end -= last.len_utf8();
        } else {
            return end;
        }
    }
}

/// The first word of a row that continues a URL wrapped from the row above, after the row's
/// indent and gutter.
fn continuation(text: &str) -> Option<Range<usize>> {
    let start = text.find(|c: char| !c.is_whitespace() && !is_gutter(c))?;
    // A gutter glyph right before the word is part of the layout, not of the URL.
    let end = start
        + text[start..]
            .find(|c: char| !is_url_char(c))
            .unwrap_or(text.len() - start);
    let end = start + trim_url_end(&text[start..end]);
    (end > start).then_some(start..end)
}

fn is_gutter(c: char) -> bool {
    matches!(c, '│' | '▏' | '▌' | '┃' | '|')
}

/// The paths in `text` that name something that exists, with the file URL each opens.
fn scan_paths(text: &str) -> Vec<(Range<usize>, String)> {
    let mut paths = Vec::new();
    let mut start = None;
    for (index, c) in text
        .char_indices()
        .chain(std::iter::once((text.len(), ' ')))
    {
        if is_path_char(c) {
            start.get_or_insert(index);
            continue;
        }
        let Some(from) = start.take() else {
            continue;
        };
        let token = &text[from..index];
        if let Some((visible, target)) = path_link(token) {
            paths.push((from..from + visible, target));
        }
    }
    paths
}

fn is_path_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '~' | '@' | '+' | ':')
}

/// Whether `token` is a path that exists, and if so how much of it is the visible link (the path
/// and its `:line:column`) and the URL it opens.
fn path_link(token: &str) -> Option<(usize, String)> {
    let token = token.trim_end_matches(['.', ',', ';', ':']);
    if token.contains("://") {
        return None;
    }
    // `src/lib.rs:40:7`: the location is shown but is not part of the file.
    let mut path = token;
    while let Some((head, tail)) = path.rsplit_once(':') {
        if tail.is_empty() || !tail.bytes().all(|byte| byte.is_ascii_digit()) {
            break;
        }
        path = head;
    }
    if path.contains(':') || path.len() < 3 {
        return None;
    }
    let looks_like_path = path.contains('/')
        || path.rsplit_once('.').is_some_and(|(stem, ext)| {
            !stem.is_empty() && ext.chars().all(|c| c.is_ascii_alphabetic()) && !ext.is_empty()
        });
    if !looks_like_path {
        return None;
    }
    let absolute = resolve(path)?;
    Some((token.len(), file_url(&absolute)))
}

/// The absolute path `path` names, if it exists. Answers are remembered, so a row drawn every
/// frame does not touch the disk every frame.
fn resolve(path: &str) -> Option<PathBuf> {
    static SEEN: OnceLock<Mutex<HashMap<String, Option<PathBuf>>>> = OnceLock::new();
    static CWD: OnceLock<Option<PathBuf>> = OnceLock::new();
    let seen = SEEN.get_or_init(Mutex::default);
    if let Ok(seen) = seen.lock()
        && let Some(answer) = seen.get(path)
    {
        return answer.clone();
    }
    let candidate = if let Some(rest) = path.strip_prefix("~/") {
        dirs::home_dir().map(|home| home.join(rest))
    } else if Path::new(path).is_absolute() {
        Some(PathBuf::from(path))
    } else {
        CWD.get_or_init(|| std::env::current_dir().ok())
            .as_ref()
            .map(|cwd| cwd.join(path))
    };
    let answer = candidate.filter(|candidate| candidate.exists());
    if let Ok(mut seen) = seen.lock() {
        if seen.len() > 4096 {
            seen.clear();
        }
        seen.insert(path.to_owned(), answer.clone());
    }
    answer
}

fn file_url(path: &Path) -> String {
    let mut url = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        if byte.is_ascii_alphanumeric() || b"/-_.~".contains(&byte) {
            url.push(char::from(byte));
        } else {
            url.push_str(&format!("%{byte:02X}"));
        }
    }
    url
}
