//! What a session still has in flight, for a frontend that must not finish before it is done.

use super::*;

/// Work that is not finished even though the last prompt was answered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PendingWork {
    /// A turn of any owner is open on the root session.
    pub root_busy: bool,
    /// Prompts waiting in the root's queue (restored prompts held for the user do not count).
    pub queued_prompts: i64,
    /// Delegated tasks that have not ended.
    pub running_tasks: i64,
    /// Ended tasks whose results are owed to the root, which they will wake.
    pub owed_results: i64,
    /// Messages queued for the session's agents.
    pub queued_mail: i64,
    /// A provider-owned child session is working or has queued work.
    pub native_children_busy: bool,
}

impl PendingWork {
    pub fn is_idle(&self) -> bool {
        *self == Self::default()
    }

    /// What is outstanding, for a person: "2 tasks still running, 1 result owed".
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        let count = |n: i64, singular: &str, plural: &str, rest: &str| {
            format!("{n} {}{rest}", if n == 1 { singular } else { plural })
        };
        if self.root_busy {
            parts.push("the root agent still working".to_owned());
        }
        if self.queued_prompts > 0 {
            parts.push(count(
                self.queued_prompts,
                "prompt",
                "prompts",
                " still queued",
            ));
        }
        if self.running_tasks > 0 {
            parts.push(count(self.running_tasks, "task", "tasks", " still running"));
        }
        if self.owed_results > 0 {
            parts.push(count(
                self.owed_results,
                "result",
                "results",
                " not yet delivered",
            ));
        }
        if self.queued_mail > 0 {
            parts.push(count(self.queued_mail, "message", "messages", " queued"));
        }
        if self.native_children_busy {
            parts.push("a provider subagent still working".to_owned());
        }
        parts.join(", ")
    }
}

impl AcpBackend {
    pub(crate) async fn pending_work(&self) -> PendingWork {
        let (root_busy, queued_prompts) = {
            let driver = self.session_driver.lock().await;
            (
                driver.has_open_turn() || driver.is_fenced(),
                driver.queue_len(),
            )
        };
        let counts = self.orchestrator.pending_counts().await;
        PendingWork {
            root_busy,
            queued_prompts: i64::try_from(queued_prompts).unwrap_or(i64::MAX),
            running_tasks: counts.running_tasks,
            owed_results: counts.owed_results,
            queued_mail: counts.queued_mail,
            native_children_busy: self.native_children.has_pending_work().await,
        }
    }
}
