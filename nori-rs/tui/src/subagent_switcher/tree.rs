//! Arranging a roster as the actor tree: `◈ main`, its children, theirs, each with its guides.
//!
//! Siblings are ordered running first (the one that did something most recently on top, fixed
//! from the first time it was seen so activity never shuffles rows under the cursor), then the
//! ended ones, the most recently ended first. An actor whose parent is not in the roster hangs
//! from the root.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::collections::HashSet;
use std::time::Instant;

use super::item::ActorItem;
use super::roster::Entry;
use super::roster::Roster;
use super::roster::Target;

/// When each running actor last did something, as of the first time it was seen.
pub(crate) type FirstSeen = HashMap<Target, Instant>;

pub(crate) fn arrange(roster: &Roster, first_seen: &mut FirstSeen) -> Vec<ActorItem> {
    for entry in std::iter::once(&roster.main).chain(&roster.subagents) {
        first_seen.entry(entry.target).or_insert(entry.active_at);
    }
    let known: HashSet<Target> = roster.subagents.iter().map(|entry| entry.target).collect();
    let mut children: HashMap<Target, Vec<&Entry>> = HashMap::new();
    for entry in &roster.subagents {
        let parent = entry
            .parent
            .as_ref()
            .and_then(|parent| parent.target)
            .map(|target| roster.resolve(target))
            .filter(|target| known.contains(target) && *target != entry.target)
            .unwrap_or(Target::Main);
        children.entry(parent).or_default().push(entry);
    }
    for siblings in children.values_mut() {
        siblings.sort_by(|a, b| natural_order(a, b, first_seen));
    }
    let mut items = vec![ActorItem {
        entry: roster.main.clone(),
        guides: String::new(),
        animations: roster.animations,
    }];
    let mut visited = HashSet::from([Target::Main]);
    walk(
        Target::Main,
        "",
        &children,
        roster.animations,
        &mut visited,
        &mut items,
    );
    items
}

fn walk(
    parent: Target,
    inherited: &str,
    children: &HashMap<Target, Vec<&Entry>>,
    animations: bool,
    visited: &mut HashSet<Target>,
    out: &mut Vec<ActorItem>,
) {
    let Some(siblings) = children.get(&parent) else {
        return;
    };
    let count = siblings.len();
    for (index, entry) in siblings.iter().enumerate() {
        if !visited.insert(entry.target) {
            continue;
        }
        let last = index + 1 == count;
        out.push(ActorItem {
            entry: (*entry).clone(),
            guides: format!("{inherited}{}", if last { "└─ " } else { "├─ " }),
            animations,
        });
        let next = format!("{inherited}{}", if last { "   " } else { "│  " });
        walk(entry.target, &next, children, animations, visited, out);
    }
}

fn natural_order(a: &Entry, b: &Entry, first_seen: &FirstSeen) -> Ordering {
    let class = |entry: &Entry| i32::from(!entry.status.is_running());
    class(a).cmp(&class(b)).then_with(|| {
        if a.status.is_running() {
            let seen = |entry: &Entry| {
                first_seen
                    .get(&entry.target)
                    .copied()
                    .unwrap_or(entry.active_at)
            };
            seen(b).cmp(&seen(a))
        } else {
            a.ended_ago.cmp(&b.ended_ago)
        }
    })
}
