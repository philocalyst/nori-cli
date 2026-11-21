//! A resume the agent cannot do itself. A user's resume of a conversation Nori recorded ends in a
//! live session: when the agent no longer has its session (`session/resume` or `session/load`
//! fails), the conversation goes on in a new session of the same agent, in the same directory. The recorded conversation is
//! drawn from the transcript and handed to the agent as context with the first prompt (the
//! summary in `pending_compact_summary`), and the model, effort and mode the conversation had
//! are put back. This module holds what that fallback says and restores.

use super::*;

/// Mark the failed live resume request so clients can pair its error with the fallback notice.
pub(super) fn bracket_failed_resume(
    events: &mut Vec<nori_protocol::SessionEvent>,
    request_id: acp::RequestId,
) {
    let Some(response_index) = events.iter().position(|event| {
        matches!(
            event,
            nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Response {
                request_id: response_id,
                response: Err(_),
            }) if response_id == &request_id
        )
    }) else {
        return;
    };
    events.insert(
        response_index,
        nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::SessionPhaseChanged(
            nori_protocol::SessionPhase::Loading {
                request_id: request_id.clone(),
            },
        )),
    );
    events.insert(
        response_index + 2,
        nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::SessionPhaseChanged(
            nori_protocol::SessionPhase::Idle,
        )),
    );
}

/// Why the agent could not reopen its session, in a line a person can read.
pub(super) fn why(error: &anyhow::Error) -> String {
    match classify_failure(error).class {
        FailureClass::SessionNotFound => "the agent no longer has that session".to_owned(),
        FailureClass::SessionNotResumable => "the agent cannot reattach it".to_owned(),
        FailureClass::Auth
        | FailureClass::RateLimited { .. }
        | FailureClass::QuotaExhausted { .. }
        | FailureClass::Capacity
        | FailureClass::ModelUnavailable
        | FailureClass::ExecutableNotFound
        | FailureClass::Initialization
        | FailureClass::PromptTooLong
        | FailureClass::ServerError
        | FailureClass::SessionAlreadyActive
        | FailureClass::NoActiveSession
        | FailureClass::Network
        | FailureClass::Unknown => {
            let text = format!("{error:#}");
            let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
            match line.char_indices().nth(160) {
                Some((end, _)) => format!("{}…", &line[..end]),
                None => line,
            }
        }
    }
}

/// What the person is told when the conversation continues in a new session.
pub(super) fn notice(reason: &str) -> String {
    format!(
        "Couldn't reopen the agent's own session ({reason}). Continuing this conversation in a \
         new session: the earlier turns are given to the agent as context with your next message."
    )
}

/// Put back the model, effort and mode `transcript` last had, on the session that continues it.
/// Best effort: a value the new session does not offer, or a failed request, keeps what the
/// session started with.
pub(super) async fn restore_dials(
    connection: &AcpConnection,
    session_id: &acp::SessionId,
    transcript: &crate::transcript::Transcript,
    nori_home: &std::path::Path,
) {
    let path = crate::transcript::TranscriptLoader::new(nori_home.to_path_buf())
        .session_path(&transcript.meta.project_id, &transcript.meta.session_id);
    let owned = transcript.clone();
    let Ok(recorded) = tokio::task::spawn_blocking(move || {
        crate::transcript::recorded_config_options(&owned, &path)
    })
    .await
    else {
        return;
    };
    let current = connection.config_options();
    for dial in [
        nori_protocol::Dial::Model,
        nori_protocol::Dial::Effort,
        nori_protocol::Dial::Mode,
    ] {
        let Some(wanted) = recorded
            .iter()
            .find(|option| dial.selects(option))
            .and_then(selected_value)
        else {
            continue;
        };
        let Some(option) = current.iter().find(|option| dial.selects(option)) else {
            continue;
        };
        if selected_value(option).as_deref() == Some(wanted.as_str()) {
            continue;
        }
        if let Err(error) = connection
            .set_config_option(session_id, option.id.clone(), wanted.clone())
            .await
        {
            warn!("Could not restore the {} {wanted}: {error}", dial.noun());
        }
    }
}

fn selected_value(option: &acp::SessionConfigOption) -> Option<String> {
    if let acp::SessionConfigKind::Select(select) = &option.kind {
        Some(select.current_value.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::bracket_failed_resume;
    use nori_protocol::AcpEvent;
    use nori_protocol::NoriEvent;
    use nori_protocol::SessionEvent;
    use nori_protocol::SessionPhase;
    use pretty_assertions::assert_eq;

    #[test]
    fn failed_resume_phase_brackets_only_its_error_response() {
        let request_id = nori_protocol::acp::v1::RequestId::Str("resume-1".to_string());
        let mut events = vec![SessionEvent::Acp(AcpEvent::Response {
            request_id: request_id.clone(),
            response: Err(nori_protocol::acp::v1::Error::new(
                -32002,
                "session not found",
            )),
        })];

        bracket_failed_resume(&mut events, request_id.clone());

        assert_eq!(
            events,
            vec![
                SessionEvent::Nori(NoriEvent::SessionPhaseChanged(SessionPhase::Loading {
                    request_id: request_id.clone(),
                })),
                SessionEvent::Acp(AcpEvent::Response {
                    request_id,
                    response: Err(nori_protocol::acp::v1::Error::new(
                        -32002,
                        "session not found"
                    )),
                }),
                SessionEvent::Nori(NoriEvent::SessionPhaseChanged(SessionPhase::Idle)),
            ]
        );
    }
}
