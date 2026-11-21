//! Shared by the fault-catalog tests: loading the showcase worlds and reading what happened.
#![allow(dead_code)]

use std::path::PathBuf;
use std::time::Duration;

use nori_world::Approvals;
use nori_world::Entry;
use nori_world::Events;
use nori_world::Record;
use nori_world::World;
use nori_world::WorldOptions;
use nori_world::WorldSession;

/// A showcase world by file name.
pub fn world(name: &str) -> World {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../fake-agent/worlds")
        .join(name);
    World::from_path(&path).unwrap_or_else(|error| panic!("{error}"))
}

/// Options for a test: a quiet moment just over the harness's completion-wake batching, and
/// a wait that fails the test instead of the suite.
pub fn options(agent: &str) -> WorldOptions {
    WorldOptions {
        agent: Some(agent.to_owned()),
        approvals: Approvals::Manual,
        quiet: Duration::from_millis(900),
        // Worlds that share an agent slug run one at a time, and a boot waits for its turn.
        timeout: Duration::from_secs(150),
        ..WorldOptions::default()
    }
}

pub async fn boot(world_file: &str, agent: &str) -> WorldSession {
    WorldSession::boot(world(world_file), options(agent))
        .await
        .unwrap_or_else(|error| panic!("boot {agent}: {error:#}"))
}

/// What the user was shown for each failed request.
pub fn failure_messages(events: &Events) -> Vec<String> {
    events
        .failures()
        .into_iter()
        .map(|failure| failure.message.clone())
        .collect()
}

/// Whether a message a person reads is a provider's JSON body handed through as it came.
pub fn is_raw_provider_json(message: &str) -> bool {
    [
        "{\"",
        "\"error\"",
        "invalid_request_error",
        "string_above_max_length",
        "overloaded_error",
    ]
    .iter()
    .any(|marker| message.contains(marker))
}

/// The prompts `agent` was refused for their size, as the provider worded it.
pub fn refusals(recorder: &[Entry], agent: &str) -> Vec<String> {
    recorder
        .iter()
        .filter_map(|entry| match &entry.record {
            Record::PromptRefused {
                agent: who,
                bytes,
                largest_block_bytes,
                ..
            } if who == agent => Some(format!(
                "{who} refused a prompt of {bytes} bytes (largest block {largest_block_bytes})"
            )),
            _ => None,
        })
        .collect()
}

/// The largest text block any prompt to `agent` carried.
pub fn largest_block(recorder: &[Entry], agent: &str) -> i64 {
    recorder
        .iter()
        .filter_map(|entry| match &entry.record {
            Record::PromptReceived {
                agent: who,
                largest_block_bytes,
                ..
            }
            | Record::PromptRefused {
                agent: who,
                largest_block_bytes,
                ..
            } if who == agent => Some(*largest_block_bytes),
            _ => None,
        })
        .max()
        .unwrap_or(0)
}

/// The value of select option `id` in a session config snapshot, read from its JSON form so a
/// test does not depend on the protocol crate's layout.
pub fn option_value(
    options: &[nori_protocol::acp::v1::SessionConfigOption],
    id: &str,
) -> Option<String> {
    options.iter().find_map(|option| {
        let json = serde_json::to_value(option).ok()?;
        (json.get("id")?.as_str()? == id)
            .then(|| json.get("currentValue")?.as_str().map(str::to_owned))
            .flatten()
    })
}

/// How many choices select option `id` offers.
pub fn option_choices(options: &[nori_protocol::acp::v1::SessionConfigOption], id: &str) -> usize {
    options
        .iter()
        .find_map(|option| {
            let json = serde_json::to_value(option).ok()?;
            (json.get("id")?.as_str()? == id)
                .then(|| json.get("options")?.as_array().map(Vec::len))
                .flatten()
        })
        .unwrap_or(0)
}

/// Everything the harness said in words: why a session ended, notices, failed requests.
pub fn words(events: &Events) -> Vec<String> {
    events
        .nori()
        .filter_map(|event| match event {
            nori_protocol::NoriEvent::SessionEnded(ended) => ended.message.clone(),
            nori_protocol::NoriEvent::Notice(notice) => Some(notice.message.clone()),
            nori_protocol::NoriEvent::RequestFailed(failure) => Some(failure.message.clone()),
            _ => None,
        })
        .collect()
}

/// Wait (in real time) until the agent `agent` has been handed a prompt containing `marker`,
/// accepted or refused; false when it never was.
pub async fn prompt_reached(session: &WorldSession, agent: &str, marker: &str) -> bool {
    for _ in 0..400 {
        let reached = session.recorder().iter().any(|entry| match &entry.record {
            Record::PromptReceived {
                agent: who, text, ..
            } => who == agent && text.contains(marker),
            Record::PromptRefused { agent: who, .. } => who == agent,
            _ => false,
        });
        if reached {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}
