//! Another actor, as its directory entry shows it: state changes, the results of its tasks, and
//! what it says to this actor.

use std::collections::HashMap;
use std::collections::HashSet;

use tokio::sync::mpsc::Sender;

use super::Item;
use crate::backend::monitor::event::Ending;
use crate::backend::monitor::spec::Track;
use crate::backend::orchestrator::ActorWatch;

/// The most of a result or a message quoted in an event.
const QUOTE_CHARS: usize = 600;

pub(super) async fn run(
    mut actor: ActorWatch,
    track: &[Track],
    tx: &Sender<Item>,
) -> Option<Ending> {
    // What is already so is the baseline, not news.
    let first = actor.observe();
    let mut state = first.state;
    let mut tasks: HashMap<String, &'static str> = first
        .tasks
        .iter()
        .map(|task| (task.id.clone(), task.status))
        .collect();
    let mut replies: HashSet<String> = first.replies.iter().map(|reply| reply.id.clone()).collect();
    let name = first.label;
    loop {
        if !actor.changed().await {
            return Some(Ending::Failed {
                reason: "the actor directory is gone".to_owned(),
            });
        }
        let now = actor.observe();
        let mut lines: Vec<(String, bool)> = Vec::new();
        if track.contains(&Track::State) && now.state != state {
            lines.push((format!("{name}: {state} -> {}", now.state), now.failed));
        }
        for task in &now.tasks {
            let previous = tasks.insert(task.id.clone(), task.status);
            if previous != Some(task.status) && task.ended && track.contains(&Track::Results) {
                let detail = quote(&task.detail);
                let text = if detail.is_empty() {
                    format!("{name}: {} ({}) {}", task.id, task.title, task.status)
                } else {
                    format!(
                        "{name}: {} ({}) {}: {detail}",
                        task.id, task.title, task.status
                    )
                };
                lines.push((text, task.failed));
            }
        }
        for reply in &now.replies {
            if replies.insert(reply.id.clone()) && track.contains(&Track::Replies) {
                lines.push((
                    format!("{name} wrote to you: {}", quote(&reply.body)),
                    false,
                ));
            }
        }
        state = now.state;
        for (text, failure) in lines {
            if tx.send(Item::Line { text, failure }).await.is_err() {
                return None;
            }
        }
        if now.finished {
            return Some(Ending::Finished {
                ok: !now.failed,
                note: format!("{name} is {state}; nothing more will happen until it is messaged."),
            });
        }
    }
}

fn quote(text: &str) -> String {
    let text = text.trim();
    let mut quoted: String = text.chars().take(QUOTE_CHARS).collect();
    if text.chars().count() > QUOTE_CHARS {
        quoted.push('…');
    }
    quoted.replace('\n', " ")
}
