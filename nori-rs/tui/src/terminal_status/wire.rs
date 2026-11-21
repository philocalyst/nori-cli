//! The wire format of the Program Status Protocol (OSC 7501), revision 0.4.
//!
//! A [`Report`] is one record: the terminal replaces the whole record with each report, so a
//! report always repeats every key the record should keep. Encoding enforces the protocol's
//! limits, so a report that leaves here is never discarded by the terminal for being too big,
//! and free text is always one line of clean UTF-8.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

/// The program's stable identifier, a grouping key for the terminal.
pub(crate) const APP: &str = "nori";

/// Largest decoded `title`: 192 bytes.
const MAX_TITLE_BYTES: usize = 192;
/// Largest decoded `msg`: 2048 bytes. Messages here stay far shorter; they are one line.
const MAX_MSG_BYTES: usize = 2048;
/// Longest `id`: 128 bytes, 8 segments of at most 32.
const MAX_ID_BYTES: usize = 128;
const MAX_ID_DEPTH: usize = 8;

/// What the program says it is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum State {
    Idle,
    Working,
    Done,
    Blocked,
    Error,
}

impl State {
    pub(crate) const ALL: [Self; 5] = [
        Self::Idle,
        Self::Working,
        Self::Done,
        Self::Blocked,
        Self::Error,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Done => "done",
            Self::Blocked => "blocked",
            Self::Error => "error",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|state| state.name() == name)
    }
}

/// What a blocked program waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Permission,
    Question,
    Auth,
}

impl Kind {
    pub(crate) const ALL: [Self; 3] = [Self::Permission, Self::Question, Self::Auth];

    fn name(self) -> &'static str {
        match self {
            Self::Permission => "permission",
            Self::Question => "question",
            Self::Auth => "auth",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }
}

/// One record, addressed by `id` (empty for the root record).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Report {
    pub(crate) id: String,
    pub(crate) state: State,
    pub(crate) kind: Option<Kind>,
    /// 0 to 100, only with `Working` and `Blocked`.
    pub(crate) progress: Option<i64>,
    pub(crate) title: Option<String>,
    pub(crate) msg: Option<String>,
}

impl Report {
    pub(crate) fn new(id: &str, state: State) -> Self {
        Self {
            id: id.to_owned(),
            state,
            kind: None,
            progress: None,
            title: None,
            msg: None,
        }
    }

    /// The sequence for this report, or `None` when the terminal cannot take it.
    pub(crate) fn encode(&self, caps: &Capabilities) -> Option<String> {
        if !valid_id(&self.id) {
            return None;
        }
        let state = caps.fallback_state(self.state)?;
        let mut body = format!("state={}:app={APP}", state.name());
        if !self.id.is_empty() {
            body.push_str(&format!(":id={}", self.id));
        }
        let blocked = state == State::Blocked;
        if blocked && let Some(kind) = self.kind.filter(|kind| caps.kinds.contains(kind)) {
            body.push_str(&format!(":kind={}", kind.name()));
        }
        if matches!(state, State::Working | State::Blocked)
            && let Some(progress) = self.progress
        {
            body.push_str(&format!(":progress={}", progress.clamp(0, 100)));
        }
        for (key, text, limit) in [
            ("title", &self.title, MAX_TITLE_BYTES),
            ("msg", &self.msg, MAX_MSG_BYTES),
        ] {
            if let Some(text) = text.as_deref().and_then(|text| one_line(text, limit)) {
                body.push_str(&format!(":{key}={}", STANDARD.encode(text)));
            }
        }
        Some(format!("\x1b]7501;{body}\x1b\\"))
    }
}

/// The sequence that removes a record and everything beneath it; the whole terminal's records
/// when `id` is empty.
pub(crate) fn clear_sequence(id: &str) -> String {
    if id.is_empty() {
        "\x1b]7501;state=clear\x1b\\".to_owned()
    } else {
        format!("\x1b]7501;state=clear:id={id}\x1b\\")
    }
}

/// The feature-detection query, followed by primary device attributes: every terminal answers
/// the latter, so its reply arriving first means the former went unanswered.
pub(crate) const QUERY: &str = "\x1b]7501;?\x1b\\\x1b[c";

fn valid_id(id: &str) -> bool {
    id.is_empty()
        || (id.len() <= MAX_ID_BYTES
            && id.split('/').count() <= MAX_ID_DEPTH
            && id.split('/').all(|segment| {
                (1..=32).contains(&segment.len())
                    && segment.bytes().all(|b| {
                        b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'+' | b'-')
                    })
            }))
}

/// Text for `title` or `msg`: control and invisible-formatting characters removed, whitespace
/// collapsed to single spaces, cut to `limit` bytes on a character boundary with an ellipsis.
fn one_line(text: &str, limit: usize) -> Option<String> {
    let mut clean = crate::terminal_title::sanitize_terminal_title(text);
    if clean.len() > limit {
        let mut end = limit.saturating_sub('…'.len_utf8());
        while !clean.is_char_boundary(end) {
            end -= 1;
        }
        clean.truncate(end);
        clean.push('…');
    }
    (!clean.is_empty()).then_some(clean)
}

/// What the terminal said it accepts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Capabilities {
    pub(crate) states: Vec<State>,
    pub(crate) kinds: Vec<Kind>,
}

impl Capabilities {
    /// What a reply without `states` or `kinds` means: all of them.
    pub(crate) fn full() -> Self {
        Self {
            states: State::ALL.to_vec(),
            kinds: Kind::ALL.to_vec(),
        }
    }

    /// Read the terminal's answer to [`QUERY`]: `? : states=a,b : kinds=c,d`. Pairs that are not
    /// understood are ignored; a missing list means the full list.
    pub(crate) fn parse_reply(body: &str) -> Option<Self> {
        let mut pairs = body.split(':').map(str::trim);
        if pairs.next()? != "?" {
            return None;
        }
        let mut caps = Self::full();
        for pair in pairs {
            match pair.split_once('=') {
                Some(("states", list)) => {
                    caps.states = list.split(',').filter_map(State::parse).collect();
                }
                Some(("kinds", list)) => {
                    caps.kinds = list.split(',').filter_map(Kind::parse).collect();
                }
                Some(_) | None => {}
            }
        }
        Some(caps)
    }

    /// The state to report instead of `state` when the terminal lacks it: a state it has, that
    /// says the nearest true thing. `None` when nothing it accepts is true.
    fn fallback_state(&self, state: State) -> Option<State> {
        let order: &[State] = match state {
            State::Idle => &[State::Idle],
            State::Working => &[State::Working, State::Idle],
            State::Done => &[State::Done, State::Idle],
            State::Blocked => &[State::Blocked, State::Working, State::Idle],
            State::Error => &[State::Error, State::Done, State::Idle],
        };
        order
            .iter()
            .copied()
            .find(|candidate| self.states.contains(candidate))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::collections::HashMap;

    fn fields(sequence: &str) -> HashMap<&str, &str> {
        sequence
            .strip_prefix("\x1b]7501;")
            .unwrap()
            .strip_suffix("\x1b\\")
            .unwrap()
            .split(':')
            .map(|pair| pair.split_once('=').unwrap())
            .collect()
    }

    #[test]
    fn encoding_is_single_line_bounded_and_utf8_safe() {
        let mut report = Report::new("s1/s2", State::Blocked);
        report.kind = Some(Kind::Permission);
        report.progress = Some(140);
        report.title = Some(format!("{}\n\u{80}x", "é".repeat(100)));
        report.msg = Some("a b ".repeat(1000));
        let sequence = report.encode(&Capabilities::full()).unwrap();
        assert!(sequence.len() < 4096);
        let fields = fields(&sequence);
        assert_eq!(fields["id"], "s1/s2");
        assert_eq!(fields["kind"], "permission");
        assert_eq!(fields["progress"], "100");
        let title = STANDARD.decode(fields["title"]).unwrap();
        assert!(title.len() <= MAX_TITLE_BYTES);
        assert!(String::from_utf8(title).unwrap().ends_with('…'));
        assert!(STANDARD.decode(fields["msg"]).unwrap().len() <= MAX_MSG_BYTES);
    }

    #[test]
    fn reply_without_lists_means_everything_and_unknown_values_are_ignored() {
        assert_eq!(Capabilities::parse_reply("?"), Some(Capabilities::full()));
        let caps =
            Capabilities::parse_reply("?:states=idle,working,paused:kinds=auth:future=1").unwrap();
        assert_eq!(caps.states, vec![State::Idle, State::Working]);
        assert_eq!(caps.kinds, vec![Kind::Auth]);
        let mut done = Report::new("", State::Done);
        done.msg = Some("ok".into());
        assert_eq!(
            done.encode(&caps).as_deref(),
            Some("\x1b]7501;state=idle:app=nori:msg=b2s=\x1b\\")
        );
    }
}
