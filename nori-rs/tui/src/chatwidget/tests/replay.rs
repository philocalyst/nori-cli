//! A replayed session renders exactly as it did live.

use super::*;
use nori_protocol::acp::v1 as acp;
use pretty_assertions::assert_eq;

fn update(update: serde_json::Value) -> nori_protocol::SessionEvent {
    let notification: acp::SessionNotification =
        serde_json::from_value(serde_json::json!({ "sessionId": "session", "update": update }))
            .expect("session notification");
    nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Notification(
        acp::AgentNotification::SessionNotification(notification),
    ))
}

fn user(text: &str) -> nori_protocol::SessionEvent {
    update(serde_json::json!({
        "sessionUpdate": "user_message_chunk",
        "content": { "type": "text", "text": text },
    }))
}

/// A chunk of the answer; the first of a run says how long its turn had run when it is replayed.
fn answer(text: &str, turn_elapsed_ms: Option<i64>) -> nori_protocol::SessionEvent {
    let mut chunk = serde_json::json!({
        "sessionUpdate": "agent_message_chunk",
        "content": { "type": "text", "text": text },
    });
    if let Some(ms) = turn_elapsed_ms {
        let mut meta = serde_json::Map::new();
        meta.insert(nori_protocol::TURN_ELAPSED_META_KEY.to_string(), ms.into());
        chunk["_meta"] = serde_json::Value::Object(meta);
    }
    update(chunk)
}

fn tool_call(
    id: &str,
    title: &str,
    kind: &str,
    input: serde_json::Value,
) -> nori_protocol::SessionEvent {
    update(serde_json::json!({
        "sessionUpdate": "tool_call",
        "toolCallId": id,
        "title": title,
        "kind": kind,
        "status": "pending",
        "rawInput": input,
    }))
}

fn tool_progress(id: &str) -> nori_protocol::SessionEvent {
    update(serde_json::json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": id,
        "status": "in_progress",
    }))
}

fn tool_done(id: &str, output: &str) -> nori_protocol::SessionEvent {
    update(serde_json::json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": id,
        "status": "completed",
        "content": [{ "type": "content", "content": { "type": "text", "text": output } }],
        "rawOutput": { "exit_code": 0 },
    }))
}

/// Four turns: text before, during and after a command; a read and some text; plain text; and
/// the read again, under the same tool call id as before, as some agents number their calls per
/// turn. `elapsed` is what each run of text says about its turn when replayed.
fn session(elapsed: Option<i64>) -> Vec<Vec<nori_protocol::SessionEvent>> {
    let read_turn = || {
        vec![
            user("TOOLS 1"),
            tool_call(
                "t0",
                "Read file0.rs",
                "read",
                serde_json::json!({ "path": "src/file0.rs" }),
            ),
            tool_progress("t0"),
            tool_done("t0", "fn main() {}"),
            answer("All tools done.\n", elapsed),
        ]
    };
    vec![
        vec![
            user("INTER 1"),
            answer("Before tool 0, thinking about things. ", elapsed),
            tool_call(
                "i0",
                "Run command 0",
                "execute",
                serde_json::json!({ "command": "echo 0" }),
            ),
            tool_progress("i0"),
            answer("(midway 0) ", elapsed),
            tool_done("i0", "result 0"),
            answer("After tool 0.\n\n", None),
        ],
        read_turn(),
        vec![user("N00002 plain"), answer("ok: N00002 plain\n", elapsed)],
        read_turn(),
    ]
}

fn rendered(rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>) -> Vec<String> {
    drain_insert_history(rx)
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect()
}

fn render_live(turns: Vec<Vec<nori_protocol::SessionEvent>>) -> Vec<String> {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual();
    let generation = chat.session_generation;
    for (index, turn) in turns.into_iter().enumerate() {
        let request_id = acp::RequestId::Str(format!("prompt-{index}"));
        chat.handle_session_event(
            generation,
            nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::SessionPhaseChanged(
                nori_protocol::SessionPhase::Prompting {
                    request_id: request_id.clone(),
                },
            )),
        );
        for event in turn {
            chat.handle_session_event(generation, event);
            chat.commit_stream_for_frame();
        }
        chat.handle_session_event(
            generation,
            nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Response {
                request_id,
                response: Ok(acp::AgentResponse::PromptResponse(
                    acp::PromptResponse::new(acp::StopReason::EndTurn),
                )),
            }),
        );
        chat.handle_session_event(
            generation,
            nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::SessionPhaseChanged(
                nori_protocol::SessionPhase::Idle,
            )),
        );
    }
    rendered(&mut rx)
}

fn render_replayed(turns: Vec<Vec<nori_protocol::SessionEvent>>) -> Vec<String> {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual();
    let generation = chat.session_generation;
    chat.handle_session_event(
        generation,
        nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::ReplayStarted(
            nori_protocol::ReplayStarted {
                source: nori_protocol::ReplaySource::Transcript,
            },
        )),
    );
    for event in turns.into_iter().flatten() {
        chat.handle_session_event(generation, event);
        chat.commit_stream_for_frame();
    }
    chat.handle_session_event(
        generation,
        nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::ReplayFinished),
    );
    rendered(&mut rx)
}

#[test]
fn a_replayed_session_renders_as_it_did_live() {
    let live = render_live(session(None));
    let replayed = render_replayed(session(Some(0)));

    assert_eq!(replayed, live);
    // The text around the command is one message, and the turn after the read shows it.
    assert!(
        live.iter()
            .any(|cell| cell.contains("(midway 0) After tool 0.")),
        "{live:#?}"
    );
    assert_eq!(
        live.iter()
            .filter(|cell| cell.contains("◈ src/file0.rs"))
            .count(),
        2,
        "{live:#?}"
    );
}

#[test]
fn a_replayed_separator_says_how_long_its_turn_ran() {
    let replayed = render_replayed(session(Some(65_000)));

    let duration = crate::status_indicator_widget::fmt_elapsed_compact(65);
    assert!(
        replayed
            .iter()
            .any(|cell| cell.trim_start_matches('\n').starts_with('─') && cell.contains(&duration)),
        "{replayed:#?}"
    );
    assert!(
        !replayed
            .iter()
            .any(|cell| cell.trim_start_matches('\n').starts_with('─')
                && !cell.contains(&duration)),
        "a separator without its duration: {replayed:#?}"
    );
}

#[test]
fn a_replay_is_bracketed_so_the_app_paints_it_at_once() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual();
    let generation = chat.session_generation;
    chat.handle_session_event(
        generation,
        nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::ReplayStarted(
            nori_protocol::ReplayStarted {
                source: nori_protocol::ReplaySource::Transcript,
            },
        )),
    );
    for event in session(Some(0)).into_iter().flatten() {
        chat.handle_session_event(generation, event);
    }
    chat.handle_session_event(
        generation,
        nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::ReplayFinished),
    );

    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        match event {
            AppEvent::HistoryReplay(boundary) => events.push(format!("{boundary:?}")),
            AppEvent::InsertHistoryCell(_) => events.push("cell".to_string()),
            _ => {}
        }
    }
    assert_eq!(
        events.first().map(String::as_str),
        Some("Started(Transcript)")
    );
    assert_eq!(events.last().map(String::as_str), Some("Finished"));
    assert!(events.len() > 2, "{events:?}");
}

#[test]
fn response_boundary_follows_stream_consolidation() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual();
    let generation = chat.session_generation;
    let boundary = nori_protocol::ConversationResponseBoundary {
        transcript_id: "session".to_owned(),
        start: 3,
        end: 9,
    };

    chat.handle_session_event(generation, answer("A **formatted** response.\n", None));
    chat.handle_session_event(
        generation,
        nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::ConversationResponseBoundary(
            boundary.clone(),
        )),
    );

    let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    let cell = events
        .iter()
        .position(|event| matches!(event, AppEvent::InsertHistoryCell(_)))
        .expect("the flushed answer is inserted");
    let consolidation = events
        .iter()
        .position(|event| matches!(event, AppEvent::ConsolidateAgentMessage { .. }))
        .expect("the streamed answer is consolidated");
    let response_boundary = events
        .iter()
        .position(|event| matches!(event, AppEvent::ResponseBoundary(_)))
        .expect("the response boundary is emitted");

    assert!(cell < consolidation, "{events:?}");
    assert!(consolidation < response_boundary, "{events:?}");
    assert!(matches!(
        &events[response_boundary],
        AppEvent::ResponseBoundary(actual) if actual == &boundary
    ));
}
