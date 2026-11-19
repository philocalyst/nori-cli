//! What a conversation checkpoint remembers about the actors around it.
//!
//! When the root session is compacted or replaced, the actors it delegated to keep running, and
//! the new conversation must still know who they are. The checkpoint carries a
//! `<subagent-continuity>` section: every actor and how to address it, the messages still waiting
//! in mailboxes, and a pinned digest of what each actor reported.

use std::sync::Arc;

use nori_protocol::acp::v1 as acp;

use super::Orchestrator;
use super::directory::Actor;
use super::directory::ActorKind;
use super::fence::LeaseFence;
use super::task::Party;
use super::task::TaskId;
use super::task::TaskStatus;

const MAX_PINNED_FACTS: usize = 10;
const MAX_PINNED_CHARS: usize = 1600;

/// A pinned summary of one actor: what it last reported, and what it touched.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ActorDigest {
    pub(crate) session_id: acp::SessionId,
    pub(crate) task_id: Option<TaskId>,
    pub(crate) alias: Option<String>,
    pub(crate) role: Option<String>,
    pub(crate) agent: String,
    pub(crate) model: Option<String>,
    pub(crate) pinned_facts: Vec<String>,
    pub(crate) files_touched: Vec<String>,
    pub(crate) latest_status: String,
    pub(crate) unread_mailbox_count: i64,
    pub(crate) fence: LeaseFence,
}

fn extract_pinned_facts(text: &str, fallback_activity: &str) -> Vec<String> {
    let mut facts = Vec::new();
    let mut total_chars = 0;
    for raw in text.lines() {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let line: String = trimmed.chars().take(240).collect();
        if total_chars + line.len() > MAX_PINNED_CHARS && !facts.is_empty() {
            break;
        }
        total_chars += line.len();
        facts.push(line);
        if facts.len() >= MAX_PINNED_FACTS {
            break;
        }
    }
    if facts.is_empty() && !fallback_activity.trim().is_empty() {
        facts.push(fallback_activity.trim().chars().take(240).collect());
    }
    facts
}

/// The digest of `actor` as of `task` (its latest one when `None`), summarizing
/// `summary_override` instead of the task's result when given.
pub(super) fn digest_of(
    actor: &Actor,
    task: Option<&TaskId>,
    summary_override: Option<&str>,
    status_override: Option<&str>,
) -> ActorDigest {
    let task = match task {
        Some(id) => actor.tasks.iter().find(|task| &task.id == id),
        None => actor.tasks.last(),
    };
    let (files_touched, activity) = task
        .and_then(|task| task.progress.lock().ok())
        .map(|progress| (progress.files_changed.clone(), progress.activity.clone()))
        .unwrap_or_default();
    let source_text = summary_override
        .filter(|text| !text.trim().is_empty())
        .or_else(|| task.map(|task| task.result.as_str()))
        .unwrap_or_default();
    ActorDigest {
        session_id: actor.wire.clone(),
        task_id: task.map(|task| task.id.clone()),
        alias: actor.aliases.first().cloned(),
        role: actor.role.clone(),
        agent: actor.agent.clone(),
        model: actor.model.clone(),
        pinned_facts: extract_pinned_facts(source_text, &activity),
        files_touched,
        latest_status: status_override.map(str::to_owned).unwrap_or_else(|| {
            task.map_or_else(
                || actor.presence().label().to_owned(),
                |task| task.status.label().to_owned(),
            )
        }),
        unread_mailbox_count: i64::try_from(
            actor
                .tasks
                .iter()
                .filter(|task| task.status == TaskStatus::Queued)
                .count(),
        )
        .unwrap_or(i64::MAX),
        fence: task.map(|task| task.fence).unwrap_or_default(),
    }
}

/// The first line of what the actor last reported, shortened: its roster headline.
fn headline(actor: &Actor) -> String {
    let reported = actor
        .tasks
        .iter()
        .rev()
        .filter(|task| matches!(task.status, TaskStatus::Ended(_)))
        .map(|task| task.result.as_str())
        .chain(
            actor
                .digest
                .iter()
                .flat_map(|digest| digest.pinned_facts.iter().map(String::as_str)),
        )
        .find_map(|text| text.lines().map(str::trim).find(|line| !line.is_empty()));
    match reported {
        Some(line) => {
            let mut short: String = line.chars().take(120).collect();
            if line.chars().count() > 120 {
                short.push('…');
            }
            short
        }
        None => "nothing reported yet".to_owned(),
    }
}

/// The roster of every actor the agent can talk to, the mail still waiting, and what each actor
/// has pinned, as a `<subagent-continuity>` section for a checkpoint. An actor's identity,
/// alias, outstanding tasks and queued mail are exactly what the directory holds, so they are
/// the same after the root session was replaced.
fn render_continuity(actors: &[&Actor]) -> Option<String> {
    let others: Vec<&&Actor> = actors
        .iter()
        .filter(|actor| actor.kind != ActorKind::Root)
        .collect();
    let mail: Vec<String> = actors
        .iter()
        .flat_map(|actor| {
            actor
                .tasks
                .iter()
                .filter(|task| task.status == TaskStatus::Queued)
                .flat_map(move |task| {
                    task.messages
                        .iter()
                        .filter_map(move |message| match &message.from {
                            Party::Actor(sender) => Some(format!(
                                "- [{}] from {sender} to actor://{}: {}",
                                message.id,
                                actor.address(),
                                message.body
                            )),
                            Party::User => None,
                        })
                })
        })
        .collect();
    if others.is_empty() && mail.is_empty() {
        return None;
    }
    let mut out = String::from("<subagent-continuity>\n");
    if !others.is_empty() {
        out.push_str("Actors you can talk to (task_send to `actor://<alias>`; an ended actor restarts with its history when messaged):\n");
        for actor in others {
            let model = actor
                .model
                .as_deref()
                .map(|model| format!(" {model}"))
                .unwrap_or_default();
            let role = actor
                .role
                .as_deref()
                .map(|role| format!(", {role}"))
                .unwrap_or_default();
            out.push_str(&format!(
                "- actor://{} (session {}) — {}{model}{role} — {} — last: {}\n",
                actor.address(),
                actor.wire,
                actor.agent,
                match actor.presence() {
                    super::directory::Presence::Ended(nori_protocol::SubagentOutcome::Failed {
                        reason: Some(reason),
                    }) => format!("failed: {reason}"),
                    other => other.label().to_owned(),
                },
                headline(actor)
            ));
            let outstanding: Vec<String> = actor
                .open_tasks()
                .map(|task| format!("{} \"{}\" ({})", task.id, task.title, task.status.label()))
                .collect();
            if !outstanding.is_empty() {
                out.push_str(&format!("  outstanding: {}\n", outstanding.join(", ")));
            }
            let files = actor
                .digest
                .as_ref()
                .map(|digest| digest.files_touched.clone())
                .unwrap_or_default();
            if !files.is_empty() {
                out.push_str(&format!("  files: {}\n", files.join(", ")));
            }
            for fact in actor
                .digest
                .iter()
                .flat_map(|digest| digest.pinned_facts.iter())
                .take(3)
            {
                out.push_str(&format!("  * {fact}\n"));
            }
        }
    }
    if !mail.is_empty() {
        out.push_str("Messages still waiting in mailboxes:\n");
        for line in mail {
            out.push_str(&line);
            out.push('\n');
        }
    }
    out.push_str("</subagent-continuity>");
    Some(out)
}

fn inject_continuity_into_checkpoint(checkpoint: &str, continuity: &str) -> String {
    if checkpoint.contains("<subagent-continuity>") {
        return checkpoint.to_owned();
    }
    if let Some(idx) = checkpoint.rfind("</conversation-checkpoint>") {
        let (head, tail) = checkpoint.split_at(idx);
        let separator = if head.ends_with('\n') { "" } else { "\n" };
        format!("{head}{separator}{continuity}\n{tail}")
    } else {
        format!("{checkpoint}\n\n{continuity}")
    }
}

impl Orchestrator {
    /// Enrich a compacted parent checkpoint with every actor, unread mail and pinned digest.
    pub(crate) fn augment_compacted_checkpoint(&self, checkpoint: &str) -> String {
        let continuity = self.inner.seed.tree.directory.with(|registry| {
            let actors: Vec<&Actor> = registry.addressable().collect();
            render_continuity(&actors)
        });
        match continuity {
            Some(continuity) => inject_continuity_into_checkpoint(checkpoint, &continuity),
            None => checkpoint.to_owned(),
        }
    }

    /// Keep every actor, alias, mailbox and digest when the root session is bound or replaced
    /// (summarize-and-swap `/compact`, or `/rewind`): the root is the same actor under a new
    /// session id.
    pub(super) fn preserve_continuity_on_bind(
        &self,
        old_session_id: Option<&acp::SessionId>,
        new_session_id: &acp::SessionId,
    ) {
        let owner = Arc::downgrade(&self.inner);
        self.inner.seed.tree.directory.with(|registry| {
            registry.bind_root(new_session_id.clone(), owner);
            if let Some(old) = old_session_id.filter(|old| *old != new_session_id) {
                for actor in registry.actors.values_mut() {
                    if actor.announce_parent.as_ref() == Some(old) {
                        actor.announce_parent = Some(new_session_id.clone());
                    }
                }
            }
        });
        if old_session_id.is_none_or(|old| old == new_session_id) {
            return;
        }
        // The tree is written whole at compaction, so a restore folds a short tail.
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move { inner.journal_snapshot().await });
        if let Some(backend) = self.inner.backend()
            && let Ok(mut pending) = backend.pending_compact_summary.try_lock()
            && let Some(existing) = pending.take()
        {
            *pending = Some(self.augment_compacted_checkpoint(&existing));
        }
    }
}
