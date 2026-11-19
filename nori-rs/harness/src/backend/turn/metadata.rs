//! Session facts the agent reports, and the turn status hints inside them.
//!
//! Metadata is accepted in every turn and never opens one. The one exception is the Nori
//! status hint: `working` declares an agent turn the agent started on its own, `idle` ends it.

use nori_protocol::AgentTurnCause;
use nori_protocol::acp::v1 as acp;

use super::super::session_reducer::ReduceOutput;
use super::ledger;
use super::ledger::EndReason;
use super::lifecycle;
use super::quiescence;
use crate::normalized::ClientEvent;
use crate::normalized::ClientEventNormalizer;
use crate::normalized::session_runtime::SessionRuntime;
use crate::normalized::turn::TurnOwner;

pub(in crate::backend) fn is_session_metadata(update: &acp::SessionUpdate) -> bool {
    matches!(
        update,
        acp::SessionUpdate::AvailableCommandsUpdate(_)
            | acp::SessionUpdate::CurrentModeUpdate(_)
            | acp::SessionUpdate::ConfigOptionUpdate(_)
            | acp::SessionUpdate::SessionInfoUpdate(_)
            | acp::SessionUpdate::UsageUpdate(_)
    )
}

pub(in crate::backend) fn reduce(
    runtime: &mut SessionRuntime,
    update: &acp::SessionUpdate,
    normalizer: &mut ClientEventNormalizer,
    out: &mut ReduceOutput,
) {
    if let acp::SessionUpdate::SessionInfoUpdate(info) = update
        && let Some(status) = info
            .meta
            .as_ref()
            .and_then(|meta| meta.get("nori"))
            .and_then(|nori| nori.get("status"))
            .and_then(serde_json::Value::as_str)
    {
        match status {
            "working" => {
                if runtime.turn.is_none() {
                    let turn = ledger::agent_turn(runtime, AgentTurnCause::Observed, out);
                    quiescence::touch(turn, out);
                }
            }
            "idle"
                if runtime
                    .turn
                    .as_ref()
                    .is_some_and(|turn| matches!(turn.owner, TurnOwner::Agent { .. })) =>
            {
                lifecycle::terminate(runtime, EndReason::AgentIdle, out);
            }
            _ => {}
        }
    }

    // Metadata during agent work is still a sign of life.
    if let Some(turn) = runtime.turn.as_mut() {
        quiescence::touch(turn, out);
    }

    let client_events = normalizer.push_session_update(update);
    for event in &client_events {
        if let ClientEvent::AgentCommandsUpdate(commands_update) = event {
            runtime.available_commands = commands_update.commands.clone();
        }
    }
    out.events.extend(client_events);
}
