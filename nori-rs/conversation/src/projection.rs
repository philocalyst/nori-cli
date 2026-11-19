use crate::Event;
use crate::EventId;
use crate::Record;
use crate::SliceRef;
use std::io;

/// The current visible branch, derived exclusively from the event log.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Projection {
    visible: Vec<Record>,
}

pub(crate) enum Change {
    Push,
    Truncate(usize),
}

impl Projection {
    pub fn visible(&self) -> &[Record] {
        &self.visible
    }

    pub fn head(&self) -> Option<EventId> {
        self.visible.last().map(|record| record.id)
    }

    /// Resolve inclusive stable identifiers against the current branch.
    pub fn slice(&self, reference: SliceRef) -> Option<Vec<Record>> {
        let start = self.visible.iter().position(|r| r.id == reference.start)?;
        let end = self.visible.iter().position(|r| r.id == reference.end)?;
        (start <= end).then(|| self.visible[start..=end].to_vec())
    }

    pub(crate) fn apply(&mut self, record: &Record) -> io::Result<()> {
        let change = self.prepare(record)?;
        self.commit(record, change);
        Ok(())
    }

    /// Validate before durable append without copying the visible transcript.
    pub(crate) fn prepare(&self, record: &Record) -> io::Result<Change> {
        match &record.event {
            Event::Rewind { head } => {
                let length = match head {
                    None => 0,
                    Some(id) => self
                        .visible
                        .iter()
                        .position(|r| r.id == *id)
                        .map(|index| index + 1)
                        .ok_or_else(|| {
                            io::Error::new(io::ErrorKind::InvalidData, "rewind head is not visible")
                        })?,
                };
                Ok(Change::Truncate(length))
            }
            Event::Fork { source } => {
                let start = self
                    .visible
                    .iter()
                    .position(|record| record.id == source.start);
                let end = self
                    .visible
                    .iter()
                    .position(|record| record.id == source.end);
                if !matches!((start, end), (Some(start), Some(end)) if start <= end) {
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

    pub(crate) fn commit(&mut self, record: &Record, change: Change) {
        match change {
            Change::Push => self.visible.push(record.clone()),
            Change::Truncate(length) => self.visible.truncate(length),
        }
    }
}
