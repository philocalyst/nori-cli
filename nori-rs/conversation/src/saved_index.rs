//! The index of a journal, saved beside it so the next open reads only what was appended since.
//!
//! A journal is append-only, so the index of its first `covered` bytes stays true for as long as
//! those bytes are the same. The saved index carries the last bytes it covered, and is used only
//! when the journal still has them at that place; anything else (a shorter journal, other
//! bytes, a format this version does not know) is ignored and the journal is indexed whole.
//!
//! Layout, little-endian: magic, `covered`, the check bytes, the names, then per record its
//! offset, line length, kind, name (an index into the names, or -1) and command id.

use crate::Kind;

const MAGIC: &[u8; 8] = b"NORIPKX\x01";

/// How many of the bytes before `covered` must match.
pub(crate) const CHECK_BYTES: i64 = 64;

/// A record as the saved index has it.
pub(crate) struct SavedSlot {
    pub(crate) offset: i64,
    pub(crate) len: i64,
    pub(crate) kind: Kind,
    pub(crate) name: Option<i32>,
    pub(crate) command_id: Option<String>,
}

pub(crate) struct SavedIndex {
    pub(crate) covered: i64,
    pub(crate) check: Vec<u8>,
    pub(crate) names: Vec<String>,
    pub(crate) slots: Vec<SavedSlot>,
}

fn kind_code(kind: Kind) -> u8 {
    match kind {
        Kind::Message => 1,
        Kind::ToolCall => 2,
        Kind::ToolResult => 3,
        Kind::Rewind => 4,
        Kind::Fork => 5,
        Kind::Operation => 6,
        Kind::Command => 7,
        Kind::SessionEvent => 8,
        Kind::Unknown => 9,
    }
}

fn kind_of_code(code: u8) -> Option<Kind> {
    Some(match code {
        1 => Kind::Message,
        2 => Kind::ToolCall,
        3 => Kind::ToolResult,
        4 => Kind::Rewind,
        5 => Kind::Fork,
        6 => Kind::Operation,
        7 => Kind::Command,
        8 => Kind::SessionEvent,
        9 => Kind::Unknown,
        _ => return None,
    })
}

impl SavedIndex {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.slots.len() * 22 + 1024);
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&self.covered.to_le_bytes());
        put_bytes(&mut out, &self.check);
        put_count(&mut out, self.names.len());
        for name in &self.names {
            put_bytes(&mut out, name.as_bytes());
        }
        put_count(&mut out, self.slots.len());
        for slot in &self.slots {
            out.extend_from_slice(&slot.offset.to_le_bytes());
            out.extend_from_slice(&slot.len.to_le_bytes());
            out.push(kind_code(slot.kind));
            out.extend_from_slice(&slot.name.unwrap_or(-1).to_le_bytes());
            match &slot.command_id {
                Some(id) => put_bytes(&mut out, id.as_bytes()),
                None => out.extend_from_slice(&(-1_i32).to_le_bytes()),
            }
        }
        out
    }

    /// Read a saved index, or `None` for anything not written by [`Self::encode`].
    pub(crate) fn decode(bytes: &[u8]) -> Option<Self> {
        let mut reader = Reader { bytes, at: 0 };
        if reader.take(MAGIC.len())? != MAGIC {
            return None;
        }
        let covered = reader.i64()?;
        if covered < 0 {
            return None;
        }
        let check = reader.bytes()?.to_vec();
        let name_count = reader.count()?;
        if name_count > reader.bytes.len().saturating_sub(reader.at) / 4 {
            return None;
        }
        let names = (0..name_count)
            .map(|_| String::from_utf8(reader.bytes()?.to_vec()).ok())
            .collect::<Option<Vec<_>>>()?;
        let count = reader.count()?;
        if count > reader.bytes.len().saturating_sub(reader.at) / 25 {
            return None;
        }
        let mut slots = Vec::with_capacity(count);
        let mut previous_end = 0;
        for _ in 0..count {
            let offset = reader.i64()?;
            let len = reader.i64()?;
            let end = offset.checked_add(len)?;
            if offset < previous_end || len < 0 || end >= covered {
                return None;
            }
            previous_end = end.checked_add(1)?;
            let kind = kind_of_code(*reader.take(1)?.first()?)?;
            let name = match reader.i32()? {
                -1 => None,
                index if usize::try_from(index).is_ok_and(|index| index < names.len()) => {
                    Some(index)
                }
                _ => return None,
            };
            let command_id = match reader.optional_bytes()? {
                Some(id) => Some(String::from_utf8(id.to_vec()).ok()?),
                None => None,
            };
            slots.push(SavedSlot {
                offset,
                len,
                kind,
                name,
                command_id,
            });
        }
        (reader.at == bytes.len()).then_some(Self {
            covered,
            check,
            names,
            slots,
        })
    }
}

fn put_count(out: &mut Vec<u8>, count: usize) {
    out.extend_from_slice(&i64::try_from(count).unwrap_or(i64::MAX).to_le_bytes());
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&i32::try_from(bytes.len()).unwrap_or(i32::MAX).to_le_bytes());
    out.extend_from_slice(bytes);
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(len)?;
        let taken = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(taken)
    }

    fn i64(&mut self) -> Option<i64> {
        Some(i64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn count(&mut self) -> Option<usize> {
        usize::try_from(self.i64()?).ok()
    }

    fn bytes(&mut self) -> Option<&'a [u8]> {
        self.optional_bytes()?
    }

    /// Length-prefixed bytes; a length of -1 is none.
    fn optional_bytes(&mut self) -> Option<Option<&'a [u8]>> {
        match self.i32()? {
            -1 => Some(None),
            len => Some(Some(self.take(usize::try_from(len).ok()?)?)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn an_index_reads_back_as_written_and_anything_else_is_refused() {
        let index = SavedIndex {
            covered: 120,
            check: b"tail".to_vec(),
            names: vec!["transcript-entry".into(), "backend-bound".into()],
            slots: vec![
                SavedSlot {
                    offset: 0,
                    len: 50,
                    kind: Kind::Operation,
                    name: Some(0),
                    command_id: None,
                },
                SavedSlot {
                    offset: 51,
                    len: 68,
                    kind: Kind::Command,
                    name: Some(1),
                    command_id: Some("turn-1".into()),
                },
            ],
        };
        let bytes = index.encode();
        let read = SavedIndex::decode(&bytes).expect("reads back");
        assert_eq!(
            (
                read.covered,
                read.check,
                read.names,
                read.slots
                    .iter()
                    .map(|slot| (
                        slot.offset,
                        slot.len,
                        slot.kind,
                        slot.name,
                        slot.command_id.clone()
                    ))
                    .collect::<Vec<_>>()
            ),
            (
                120,
                b"tail".to_vec(),
                vec!["transcript-entry".to_owned(), "backend-bound".to_owned()],
                vec![
                    (0, 50, Kind::Operation, Some(0), None),
                    (51, 68, Kind::Command, Some(1), Some("turn-1".to_owned())),
                ]
            )
        );
        assert!(SavedIndex::decode(&bytes[..bytes.len() - 1]).is_none());
        assert!(SavedIndex::decode(b"NORIPKX\x02").is_none());
    }

    #[test]
    fn corrupt_counts_are_rejected_without_allocating_from_them() {
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&0_i64.to_le_bytes());
        bytes.extend_from_slice(&0_i32.to_le_bytes());
        bytes.extend_from_slice(&i64::MAX.to_le_bytes());
        assert!(SavedIndex::decode(&bytes).is_none());
    }

    #[test]
    fn slots_outside_the_journal_are_rejected() {
        let index = SavedIndex {
            covered: 10,
            check: Vec::new(),
            names: Vec::new(),
            slots: vec![SavedSlot {
                offset: 9,
                len: i64::MAX,
                kind: Kind::Message,
                name: None,
                command_id: None,
            }],
        };
        assert!(SavedIndex::decode(&index.encode()).is_none());
    }
}
