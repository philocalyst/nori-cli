//! What Nori tells the terminal about the conversation: the Program Status Protocol (OSC 7501).
//!
//! The root conversation is the terminal's root record and each subagent is a child record
//! (`s1`, `s1/s2`, ...) named as Nori names it. This module keeps a [`Node`] for each of them,
//! updates it from the session's events, and hands the terminal a [`Report`] whenever what the
//! terminal should show changes. Writing is [`crate::terminal_status`]'s job; this module never
//! touches the terminal.
//!
//! * State changes go out at once. Changes to what a record says (`msg`, `progress`) are held to
//!   one report per record every [`MIN_INTERVAL`], the newest wins, so streaming never floods.
//! * A finished turn leaves `done` (or `error`) behind for the user to find; the next key press
//!   or turn retires it. Finished children are retired with it.
//! * Nothing is left behind: a new session, a closed subagent, quitting, suspending and a crash
//!   all clear what they own. See `docs/terminal-integration.md`.

mod events;
mod node;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::time::Duration;
use std::time::Instant;

use nori_protocol::acp::v1::RequestId;
use nori_protocol::acp::v1::SessionId;

use self::node::Block;
use self::node::BlockKey;
use self::node::Node;
use self::node::Phase;
use super::App;
use crate::terminal_status;
use crate::terminal_status::Kind;

/// The least time between two reports that only change what a record says.
const MIN_INTERVAL: Duration = Duration::from_millis(500);
/// Records the terminal must support: 64. Nori stays well under what it may hold: 256.
const MAX_CHILDREN: usize = 255;
const MAX_ID_BYTES: usize = 128;
const MAX_DEPTH: usize = 8;

#[derive(Default)]
pub(super) struct ProgramStatus {
    root: Option<SessionId>,
    /// Every record, by id (empty for the root). Parents sort before their children.
    nodes: BTreeMap<String, Node>,
    children: HashMap<SessionId, String>,
    requests: HashMap<RequestId, String>,
    next_child: i64,
    replaying: bool,
    /// Where the root's label comes from, best first.
    session_title: Option<String>,
    prompt_summary: Option<String>,
    project: Option<String>,
}

impl ProgramStatus {
    fn path_of(&self, session: &SessionId) -> Option<String> {
        if self.root.as_ref() == Some(session) {
            Some(String::new())
        } else {
            self.children.get(session).cloned()
        }
    }

    fn root_title(&self) -> Option<String> {
        self.session_title
            .clone()
            .or_else(|| self.prompt_summary.clone())
            .or_else(|| self.project.clone())
    }

    /// Report `path` if what the terminal should show changed.
    fn sync(&mut self, path: &str) {
        let Some(node) = self.nodes.get_mut(path) else {
            return;
        };
        let report = node.report();
        if node.sent.as_ref() == Some(&report) {
            node.dirty = false;
            return;
        }
        let now = Instant::now();
        let quiet = node.sent.as_ref().is_some_and(|sent| {
            sent.state == report.state && sent.kind == report.kind && sent.title == report.title
        });
        if quiet && node.due(MIN_INTERVAL).is_some_and(|due| now < due) {
            node.dirty = true;
            return;
        }
        terminal_status::send(&report);
        node.sent = Some(report);
        node.sent_at = Some(now);
        node.dirty = false;
    }

    /// Say the records whose reports were held back by the rate limit, when they are due.
    /// Returns how long until the next one is, if any is waiting.
    pub(super) fn tick(&mut self) -> Option<Duration> {
        if terminal_status::take_retracted() {
            self.republish();
        }
        let now = Instant::now();
        let mut next: Option<Duration> = None;
        let held: Vec<String> = self
            .nodes
            .values()
            .filter(|node| node.dirty)
            .map(|node| node.path.clone())
            .collect();
        for path in held {
            self.sync(&path);
            if let Some(node) = self.nodes.get(&path)
                && node.dirty
                && let Some(due) = node.due(MIN_INTERVAL)
            {
                let wait = due.saturating_duration_since(now);
                next = Some(next.map_or(wait, |soonest| soonest.min(wait)));
            }
        }
        next
    }

    /// Say every record again, after the terminal was handed back and they were withdrawn.
    fn republish(&mut self) {
        for node in self.nodes.values_mut() {
            let report = node.report();
            terminal_status::send(&report);
            node.sent = Some(report);
            node.sent_at = Some(Instant::now());
            node.dirty = false;
        }
    }

    /// Forget `path` and everything beneath it, and tell the terminal.
    fn drop_subtree(&mut self, path: &str) {
        let prefix = format!("{path}/");
        self.nodes
            .retain(|id, _| id != path && !id.starts_with(&prefix));
        self.children
            .retain(|_, id| id != path && !id.starts_with(&prefix));
        self.requests
            .retain(|_, id| id != path && !id.starts_with(&prefix));
        terminal_status::clear(path);
    }

    /// Retire the children whose work is over and has been seen: the next turn starts clean.
    fn drop_finished_children(&mut self) {
        let finished: Vec<String> = self
            .nodes
            .values()
            .filter(|node| {
                !node.path.is_empty() && matches!(node.phase, Phase::Done | Phase::Error)
            })
            .map(|node| node.path.clone())
            .collect();
        for path in finished {
            if self.nodes.contains_key(&path) {
                self.drop_subtree(&path);
            }
        }
    }

    /// Withdraw everything this process told the terminal.
    pub(super) fn clear(&mut self) {
        if self.nodes.values().any(|node| node.sent.is_some()) {
            terminal_status::clear("");
        }
        self.nodes.clear();
        self.children.clear();
        self.requests.clear();
        self.root = None;
        self.replaying = false;
    }

    /// The root is at rest.
    pub(super) fn idle(&mut self) {
        self.nodes
            .entry(String::new())
            .or_insert_with(|| Node::new(""));
        self.refresh_root_title();
    }

    fn refresh_root_title(&mut self) {
        let title = self.root_title();
        if let Some(root) = self.nodes.get_mut("") {
            root.title = title;
        }
        self.sync("");
    }

    /// The user is back and has seen the result: a finished root rests, finished children go.
    pub(super) fn acknowledge(&mut self) {
        if let Some(root) = self.nodes.get_mut("")
            && matches!(root.phase, Phase::Done | Phase::Error)
        {
            root.rest();
            self.sync("");
        }
        self.drop_finished_children();
    }

    /// A sign-in begins, for the root or for the subagent `session`.
    pub(super) fn auth(&mut self, session: Option<&SessionId>) {
        let path = match session {
            Some(session) => match self.children.get(session) {
                Some(path) => path.clone(),
                None => return,
            },
            None => String::new(),
        };
        let Some(node) = self.nodes.get_mut(&path) else {
            return;
        };
        node.add_block(Block {
            key: BlockKey::Auth,
            kind: Kind::Auth,
            msg: "Sign in to continue".to_owned(),
        });
        self.sync(&path);
    }

    /// The sign-in ended, finished or not.
    pub(super) fn finish_auth(&mut self) {
        let paths: Vec<String> = self.nodes.keys().cloned().collect();
        for path in paths {
            if let Some(node) = self.nodes.get_mut(&path)
                && node.blocks.iter().any(|block| block.key == BlockKey::Auth)
            {
                node.remove_block(&BlockKey::Auth);
                self.sync(&path);
            }
        }
    }

    /// The user answered a request, or it no longer needs an answer.
    pub(super) fn answered(&mut self, id: &RequestId) {
        if let Some(path) = self.requests.remove(id)
            && let Some(node) = self.nodes.get_mut(&path)
        {
            node.remove_block(&BlockKey::Request(id.clone()));
            self.sync(&path);
        }
    }
}

impl Drop for ProgramStatus {
    fn drop(&mut self) {
        self.clear();
    }
}

impl App {
    /// Between events: say what was held back by the rate limit and what the terminal was handed
    /// back without, and send the reports no frame is about to carry.
    pub(super) fn sync_program_status(&mut self, tui: &mut crate::tui::Tui) {
        if let Some(wait) = self.program_status.tick() {
            tui.frame_requester().schedule_frame_in(wait);
        }
        tui.flush_status();
    }
}
