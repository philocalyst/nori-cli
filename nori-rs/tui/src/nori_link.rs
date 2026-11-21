//! Links Nori draws to itself (`nori://`): a subagent's name in the lanes opens the subagent when
//! clicked, while the terminal keeps its own selection over everything (no mouse capture).
//!
//! A terminal does not hand a click on a link to the program that drew it, so these links are
//! drawn only when something routes the click back (`set_routed`): Rex reports clicks on custom
//! schemes as events, which Nori receives for its own pane and turns into
//! [`crate::app_event::AppEvent::OpenNoriLink`]. Every link carries this process's token, so a
//! link left in the scrollback by another Nori, or an earlier run, opens nothing here.

use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use crate::subagent_switcher::SubagentKey;

const SCHEME: &str = "nori://";

/// Something in Nori a link opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoriLink {
    Subagent(SubagentKey),
}

/// Clicks on Nori's links reach Nori.
static ROUTED: AtomicBool = AtomicBool::new(false);

/// This process's token.
fn token() -> &'static str {
    static TOKEN: OnceLock<String> = OnceLock::new();
    TOKEN.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        format!("{:x}{nanos:x}", std::process::id())
    })
}

/// Whether Nori's links are drawn: only while their clicks are routed back.
pub(crate) fn routed() -> bool {
    ROUTED.load(Ordering::Relaxed)
}

/// Clicks on Nori's links are (or are no longer) routed back to it.
pub(crate) fn set_routed(routed: bool) {
    ROUTED.store(routed, Ordering::Relaxed);
}

impl NoriLink {
    pub(crate) fn url(self) -> String {
        match self {
            Self::Subagent(key) => format!("{SCHEME}{}/subagent/{}", token(), key.0),
        }
    }

    /// The link `url` stands for, when it is one of this process's.
    pub(crate) fn parse(url: &str) -> Option<Self> {
        let (owner, rest) = url.strip_prefix(SCHEME)?.split_once('/')?;
        if owner != token() {
            return None;
        }
        match rest.split_once('/')? {
            ("subagent", key) => key.parse().ok().map(|key| Self::Subagent(SubagentKey(key))),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn a_link_opens_what_it_was_drawn_for_and_nothing_from_elsewhere() {
        let link = NoriLink::Subagent(SubagentKey(7));
        assert_eq!(NoriLink::parse(&link.url()), Some(link));
        assert_eq!(NoriLink::parse("nori://someone-else/subagent/7"), None);
        assert_eq!(NoriLink::parse("https://example.com"), None);
    }
}
