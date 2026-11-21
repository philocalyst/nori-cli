//! Build a child transcript from its canonical replay stream.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use nori_protocol::AcpEvent;
use nori_protocol::ConversationResponseBoundary;
use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;
use nori_protocol::acp::v1::AgentNotification;

use super::node::Node;
use crate::history_cell::HistoryCell;

/// A response cell's index among the projected cells, with the journal range it came from.
pub(crate) type ResponseSource = (usize, ConversationResponseBoundary);

/// A child's transcript cells, and the source of each response among them.
pub(crate) type ChildHistory = (Vec<Arc<dyn HistoryCell>>, Vec<ResponseSource>);

/// Replay the same child update parser used by the live view, associating response sources by
/// event order rather than comparing message text. Tool input and output remain in the transcript.
pub(crate) fn project_child_history(events: &[SessionEvent], cwd: &Path) -> ChildHistory {
    let mut node = Node::new(Instant::now());
    let mut boundaries = Vec::new();
    for event in events {
        match event {
            SessionEvent::Acp(AcpEvent::Notification(AgentNotification::SessionNotification(
                notification,
            ))) => node.apply(&notification.update, cwd, false),
            SessionEvent::Nori(NoriEvent::ConversationResponseBoundary(boundary)) => {
                if let Some(index) = node.replay_response_boundary() {
                    boundaries.push((index, boundary.clone()));
                }
            }
            _ => {}
        }
    }
    let (cells, _) = node.take_detail(cwd);
    (cells, boundaries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nori_protocol::acp::v1 as acp;
    use pretty_assertions::assert_eq;

    #[test]
    fn response_boundaries_follow_event_order_even_for_identical_answers() {
        let text = |value: &str| {
            SessionEvent::Acp(AcpEvent::Notification(
                AgentNotification::SessionNotification(acp::SessionNotification::new(
                    "child",
                    acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                        acp::ContentBlock::Text(acp::TextContent::new(value)),
                    )),
                )),
            ))
        };
        let boundary = |end| {
            SessionEvent::Nori(NoriEvent::ConversationResponseBoundary(
                ConversationResponseBoundary {
                    transcript_id: "journal".into(),
                    start: 1,
                    end,
                },
            ))
        };
        let (cells, sources) = project_child_history(
            &[text("same"), boundary(3), text("same"), boundary(9)],
            Path::new("."),
        );
        assert_eq!(cells.len(), 2);
        assert_eq!(
            sources,
            vec![
                (
                    0,
                    ConversationResponseBoundary {
                        transcript_id: "journal".into(),
                        start: 1,
                        end: 3
                    }
                ),
                (
                    1,
                    ConversationResponseBoundary {
                        transcript_id: "journal".into(),
                        start: 1,
                        end: 9
                    }
                ),
            ]
        );
    }
}
