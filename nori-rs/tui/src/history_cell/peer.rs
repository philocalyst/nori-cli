//! Things that happen between agents and around the turn, each drawn as one calm row.
//!
//! * `✉ reviewer → main · delivered` and the message's first words: a peer message;
//! * `⇄ claude-code/sonnet → codex/gpt-5 · quota exhausted`: a model route that changed, so a
//!   session is never silently running on something other than what it shows;
//! * `↻ Retrying in 4s · 2/4 · overloaded`: a retry that is waiting;
//! * `◈ The agent started a turn · unprompted`: work nobody asked this client for.

use std::time::Duration;

use ratatui::text::Line;
use ratatui::text::Span;

use super::HistoryCell;
use super::chrome::Chrome;
use super::chrome::Disclosure;
use super::chrome::Header;
use super::chrome::Target;
use super::chrome::format_duration;
use crate::wrapping::RtOptions;
use crate::wrapping::word_wrap_lines;

/// How far a peer message has got. Nothing reports `Delivered` while it is only queued.
///
/// Read from the message's `delivery` (`chatwidget::subagents::tree::peer_delivery`); a
/// provider's own session message carries none and counts as delivered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum PeerDelivery {
    /// Waiting in the recipient's mailbox.
    Queued,
    /// The recipient's turn contained it.
    Delivered,
    /// The recipient replied to it.
    Acknowledged,
    /// It will not be answered: its task failed, was cancelled or could not be revived. The
    /// reason is on the [`PeerHandle`].
    Failed,
}

impl PeerDelivery {
    fn label(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Delivered => "delivered",
            Self::Acknowledged => "acked",
            Self::Failed => "failed",
        }
    }
}

/// A peer message's delivery state, shared by its card and whoever hears of its progress. The
/// harness announces the same message again as it advances; the card in the transcript reads
/// the state each time it is drawn, so it moves on without being replaced. It only moves
/// forward.
#[derive(Clone, Debug)]
pub(crate) struct PeerHandle {
    state: std::sync::Arc<std::sync::atomic::AtomicU8>,
    reason: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

impl PeerHandle {
    pub(crate) fn new(delivery: PeerDelivery) -> Self {
        Self {
            state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(delivery as u8)),
            reason: std::sync::Arc::default(),
        }
    }

    pub(crate) fn get(&self) -> PeerDelivery {
        match self.state.load(std::sync::atomic::Ordering::Relaxed) {
            0 => PeerDelivery::Queued,
            1 => PeerDelivery::Delivered,
            2 => PeerDelivery::Acknowledged,
            _ => PeerDelivery::Failed,
        }
    }

    pub(crate) fn advance(&self, delivery: PeerDelivery) {
        self.state
            .fetch_max(delivery as u8, std::sync::atomic::Ordering::Relaxed);
    }

    /// The message will not be answered, and why.
    pub(crate) fn fail(&self, reason: Option<String>) {
        *self
            .reason
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = reason;
        self.advance(PeerDelivery::Failed);
    }

    /// Whether nothing more will be said about it: acknowledged or failed.
    pub(crate) fn is_settled(&self) -> bool {
        matches!(
            self.get(),
            PeerDelivery::Acknowledged | PeerDelivery::Failed
        )
    }
}

/// One end of a peer message: what the actor is called, and what runs it when that is known,
/// which colours the name.
#[derive(Clone, Debug)]
pub(crate) struct PeerParty {
    pub(crate) name: String,
    pub(crate) agent: Option<String>,
    pub(crate) model: Option<String>,
}

impl PeerParty {
    /// The name in the style of the provider behind it, bold.
    fn name_span(&self, chrome: &Chrome) -> Span<'static> {
        let style = chrome
            .identity(self.agent.as_deref(), self.model.as_deref())
            .first()
            .map_or_else(|| chrome.strong(), |model| model.style)
            .add_modifier(ratatui::style::Modifier::BOLD);
        Span::styled(self.name.clone(), style)
    }
}

/// A message from one actor to another, with the start of what it said.
#[derive(Debug)]
pub(crate) struct PeerMessageCell {
    from: PeerParty,
    to: PeerParty,
    delivery: PeerHandle,
    body: String,
    expanded: std::sync::atomic::AtomicBool,
    /// When the card was made, and whether it has been drawn in a final state: it is redrawn
    /// every frame until it has, so a state that moves on while the card is on screen shows.
    created: std::time::Instant,
    drawn_settled: std::sync::atomic::AtomicBool,
}

/// Rows of the body a collapsed card shows.
const PREVIEW_ROWS: usize = 2;
/// Rows of the body an expanded card shows at most.
const EXPANDED_ROWS: usize = 40;

impl PeerMessageCell {
    pub(crate) fn new(
        from: PeerParty,
        to: PeerParty,
        delivery: PeerHandle,
        body: impl Into<String>,
    ) -> Self {
        Self {
            from,
            to,
            delivery,
            body: body.into(),
            expanded: std::sync::atomic::AtomicBool::new(false),
            created: std::time::Instant::now(),
            drawn_settled: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn render(&self, width: u16, disclosure: Disclosure) -> Vec<Line<'static>> {
        let chrome = Chrome::live();
        let width = usize::from(width);
        let delivery = self.delivery.get();
        if self.delivery.is_settled() {
            self.drawn_settled
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let tone = match delivery {
            PeerDelivery::Queued => chrome.amber(),
            PeerDelivery::Delivered => chrome.cyan(),
            PeerDelivery::Acknowledged => chrome.mint(),
            PeerDelivery::Failed => chrome.coral(),
        };
        let reason = self
            .delivery
            .reason
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut lines = vec![chrome.header(
            Header {
                glyph: Span::styled("✉", tone),
                verb: String::new(),
                target: Target::Spans(vec![
                    self.from.name_span(&chrome),
                    Span::styled(" → ", chrome.faint()),
                    self.to.name_span(&chrome),
                ]),
                chip: Vec::new(),
                meta: {
                    let mut meta = vec![Span::styled(delivery.label(), tone)];
                    if let Some(reason) = reason.filter(|_| delivery == PeerDelivery::Failed) {
                        meta.push(Span::styled(reason, chrome.coral()));
                    }
                    meta
                },
            },
            width,
        )];
        let body = self.body.trim();
        if body.is_empty() {
            return lines;
        }
        let prefix = chrome.body_prefix();
        let quiet = chrome
            .secondary()
            .add_modifier(ratatui::style::Modifier::ITALIC);
        let wrapped = word_wrap_lines(
            body.lines()
                .map(|line| Line::from(Span::styled(line.to_owned(), quiet))),
            RtOptions::new(width.saturating_sub(chrome.body_indent()).max(1)),
        );
        let limit = match disclosure {
            Disclosure::Collapsed => PREVIEW_ROWS,
            Disclosure::Expanded => EXPANDED_ROWS,
        };
        let hidden = wrapped.len().saturating_sub(limit);
        lines.extend(wrapped.into_iter().take(limit).map(|line| {
            let mut spans = vec![prefix.clone()];
            spans.extend(line.spans);
            Line::from(spans)
        }));
        if hidden > 0 {
            lines.push(chrome.more_row(hidden, "lines"));
        }
        lines
    }
}

impl HistoryCell for PeerMessageCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let expanded = self.expanded.load(std::sync::atomic::Ordering::Relaxed);
        self.render(
            width,
            if expanded {
                Disclosure::Expanded
            } else {
                Disclosure::Collapsed
            },
        )
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.render(width, Disclosure::Expanded)
    }

    /// Its state moves while it is on screen: until it is drawn settled (or has had two minutes
    /// to settle) it is laid out again every frame.
    fn animated(&self) -> bool {
        !self
            .drawn_settled
            .load(std::sync::atomic::Ordering::Relaxed)
            && self.created.elapsed() < std::time::Duration::from_secs(120)
    }

    fn expansion(&self) -> Option<bool> {
        Some(self.expanded.load(std::sync::atomic::Ordering::Relaxed))
    }

    fn set_expanded(&self, expanded: bool) {
        self.expanded
            .store(expanded, std::sync::atomic::Ordering::Relaxed);
    }
}

/// What changed about the route a session runs on, or is waiting to retry on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RouteNotice {
    /// The session moved to another agent or model.
    Changed {
        from: RouteEnd,
        to: RouteEnd,
        reason: String,
    },
    /// The failed attempt's output above is obsolete: the request starts over.
    Superseded { retried: bool },
    /// A request failed in a way worth another try; the next attempt is `wait` away.
    Retrying {
        attempt: i32,
        max_attempts: i32,
        wait: Duration,
        reason: String,
    },
}

/// One side of a route change: the agent and the model on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RouteEnd {
    pub(crate) agent: String,
    pub(crate) model: Option<String>,
    /// Whose quota the route spends, as `Claude · secondary`.
    pub(crate) account: Option<String>,
}

/// What a route change says about accounts: nothing when both ends spend from one account,
/// else `account primary → secondary` within a provider and the full names across providers.
fn account_move(from: &RouteEnd, to: &RouteEnd) -> Option<String> {
    let (from, to) = (from.account.as_deref()?, to.account.as_deref()?);
    if from == to {
        return None;
    }
    match (from.split_once(" · "), to.split_once(" · ")) {
        (Some((from_provider, from_account)), Some((to_provider, to_account)))
            if from_provider == to_provider =>
        {
            Some(format!("account {from_account} → {to_account}"))
        }
        _ => Some(format!("{from} → {to}")),
    }
}

/// A route change or retry as the one row it is.
#[derive(Debug)]
pub(crate) struct RouteNoticeCell {
    notice: RouteNotice,
}

impl RouteNoticeCell {
    pub(crate) fn new(notice: RouteNotice) -> Self {
        Self { notice }
    }
}

impl HistoryCell for RouteNoticeCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let chrome = Chrome::live();
        let width = usize::from(width);
        let line = match &self.notice {
            RouteNotice::Changed { from, to, reason } => chrome.header(
                Header {
                    glyph: Span::styled("⇄", chrome.amber()),
                    verb: String::new(),
                    target: Target::Spans({
                        // Where it was is dimmed; where it is now is not.
                        let mut spans: Vec<Span<'static>> = chrome
                            .identity(Some(&from.agent), from.model.as_deref())
                            .into_iter()
                            .map(|mut span| {
                                span.style = span.style.add_modifier(ratatui::style::Modifier::DIM);
                                span
                            })
                            .collect();
                        spans.push(Span::styled(" → ", chrome.faint()));
                        spans.extend(chrome.identity(Some(&to.agent), to.model.as_deref()));
                        spans
                    }),
                    chip: Vec::new(),
                    meta: std::iter::once(chrome.fact(reason.clone()))
                        .chain(account_move(from, to).map(|moved| chrome.fact(moved)))
                        .collect(),
                },
                width,
            ),
            RouteNotice::Superseded { retried } => chrome.header(
                Header {
                    glyph: Span::styled("↻", chrome.faint()),
                    verb: String::new(),
                    target: Target::Spans(vec![Span::styled(
                        if *retried {
                            "retried"
                        } else {
                            "continued on another route"
                        },
                        chrome.muted(),
                    )]),
                    chip: Vec::new(),
                    meta: vec![chrome.fact("the output above is from the failed attempt")],
                },
                width,
            ),
            RouteNotice::Retrying {
                attempt,
                max_attempts,
                wait,
                reason,
            } => chrome.header(
                Header {
                    glyph: Span::styled("↻", chrome.amber()),
                    verb: "Retrying".to_owned(),
                    target: Target::Text(
                        format_duration(*wait)
                            .map_or_else(String::new, |wait| format!("in {wait}")),
                    ),
                    chip: Vec::new(),
                    meta: vec![
                        chrome.fact(format!("{attempt}/{max_attempts}")),
                        chrome.fact(reason.clone()),
                    ],
                },
                width,
            ),
        };
        vec![line]
    }
}

/// Why an agent is working without a request from this client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgentTurnKind {
    /// Work that outlived the request that started it.
    Detached,
    /// A turn started elsewhere that this client is watching.
    Observed,
    /// Work nobody asked for.
    Unprompted,
}

/// `◈ The agent is working · unprompted`, once, where an agent-owned turn begins.
#[derive(Debug)]
pub(crate) struct AgentTurnCell {
    kind: AgentTurnKind,
}

impl AgentTurnCell {
    pub(crate) fn new(kind: AgentTurnKind) -> Self {
        Self { kind }
    }
}

impl HistoryCell for AgentTurnCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let chrome = Chrome::live();
        let cause = match self.kind {
            AgentTurnKind::Detached => "continuing after your request",
            AgentTurnKind::Observed => "started elsewhere",
            AgentTurnKind::Unprompted => "unprompted",
        };
        vec![chrome.header(
            Header {
                glyph: Span::styled("◈", chrome.violet()),
                verb: String::new(),
                target: Target::Text("The agent is working".to_owned()),
                chip: Vec::new(),
                meta: vec![chrome.fact(cause)],
            },
            usize::from(width),
        )]
    }
}
