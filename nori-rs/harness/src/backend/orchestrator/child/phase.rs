//! Correlate reducer completion with the child's currently owned request.
use nori_protocol::SessionPhase;
use nori_protocol::acp::v1 as acp;

/// A forced cancellation may have no wire response after its request future is
/// aborted. Only accept Idle after observing this request's own Prompting phase;
/// an earlier turn's buffered Idle must not finish its successor.
pub(super) fn interrupted_turn_ended(
    observed: &mut bool,
    request_id: &acp::RequestId,
    interrupted: bool,
    phase: &SessionPhase,
) -> bool {
    match phase {
        SessionPhase::Prompting {
            request_id: started,
        } if started == request_id => {
            *observed = true;
            false
        }
        SessionPhase::Idle => interrupted && *observed,
        SessionPhase::Prompting { .. }
        | SessionPhase::Cancelling { .. }
        | SessionPhase::Loading { .. }
        | SessionPhase::Agent { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forced_cancel_completes_without_wire_response_but_prior_idle_does_not() {
        let request_id = acp::RequestId::Str("successor".into());
        let mut observed = false;
        assert!(!interrupted_turn_ended(
            &mut observed,
            &request_id,
            true,
            &SessionPhase::Idle
        ));
        assert!(!interrupted_turn_ended(
            &mut observed,
            &request_id,
            true,
            &SessionPhase::Prompting {
                request_id: acp::RequestId::Str("previous".into())
            }
        ));
        assert!(!interrupted_turn_ended(
            &mut observed,
            &request_id,
            true,
            &SessionPhase::Idle
        ));
        assert!(!interrupted_turn_ended(
            &mut observed,
            &request_id,
            true,
            &SessionPhase::Prompting {
                request_id: request_id.clone()
            }
        ));
        assert!(!interrupted_turn_ended(
            &mut observed,
            &request_id,
            false,
            &SessionPhase::Idle
        ));
        assert!(interrupted_turn_ended(
            &mut observed,
            &request_id,
            true,
            &SessionPhase::Idle
        ));
    }
}
