use crate::Event;
use crate::EventId;
use crate::SliceRef;
use std::io;

/// The current visible branch, derived exclusively from the event log: the ids of its records,
/// oldest first. Ids only ever grow along a branch (a rewind cuts it back to an earlier record,
/// and what is appended after has a higher id), so the branch is sorted and a record's place on
/// it is found by halving.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Projection {
    visible: Vec<EventId>,
}

pub(crate) enum Change {
    Push,
    Truncate(usize),
}

impl Projection {
    pub fn visible(&self) -> &[EventId] {
        &self.visible
    }

    pub fn head(&self) -> Option<EventId> {
        self.visible.last().copied()
    }

    /// Where on the branch the record `id` is.
    pub(crate) fn position(&self, id: EventId) -> Option<usize> {
        self.visible.binary_search(&id).ok()
    }

    /// The visible ids from `reference.start` to `reference.end`, inclusive, if both are on the
    /// branch in that order.
    pub(crate) fn slice(&self, reference: SliceRef) -> Option<&[EventId]> {
        let start = self.position(reference.start)?;
        let end = self.position(reference.end)?;
        (start <= end).then(|| &self.visible[start..=end])
    }

    /// Validate before durable append without copying the visible transcript.
    pub(crate) fn prepare(&self, event: &Event) -> io::Result<Change> {
        match event {
            Event::Rewind { head } => {
                let length = match head {
                    None => 0,
                    Some(id) => self.position(*id).map(|index| index + 1).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "rewind head is not visible")
                    })?,
                };
                Ok(Change::Truncate(length))
            }
            Event::Fork { source } => {
                if self.slice(*source).is_none() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "fork slice is not visible",
                    ));
                }
                Ok(Change::Push)
            }
            Event::Message { .. }
            | Event::ToolCall { .. }
            | Event::ToolResult { .. }
            | Event::Operation { .. }
            | Event::Command { .. }
            | Event::SessionEvent { .. }
            | Event::Unknown { .. } => Ok(Change::Push),
        }
    }

    pub(crate) fn commit(&mut self, id: EventId, change: Change) {
        match change {
            Change::Push => self.visible.push(id),
            Change::Truncate(length) => self.visible.truncate(length),
        }
    }
}
