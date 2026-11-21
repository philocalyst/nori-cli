//! The on-disk digest of one session: what a search and a preview need, without the protocol
//! traffic that makes up most of a raw transcript.
//!
//! A session's index lives in four files under `index/v1/<project>/`, each written for what an
//! update of a session that grew has to change:
//!
//! - `<session>.nidx`: the turns that are final, one [`DigestTurn`] per JSON line. Only ever
//!   appended to.
//! - `<session>.tri`: the trigram bloom filter ([`super::bloom::Bloom`]); a grown session sets a
//!   few more bits, patched in place.
//! - `<session>.nst`: the cursor ([`super::cursor::Cursor`]): how far the session was read, how
//!   many bytes of `.nidx` are valid, and the turns not final yet (the last, which may still
//!   grow). Small, and replaced whole.
//! - `<session>.nhd`: the [`DigestHeader`], a small JSON document replaced whole, written last:
//!   it is what makes an update visible to the catalog.
//!
//! A digest is the header, the turns of `.nidx` up to the length the cursor names, and the
//! cursor's unfinished turns. Small files are written to a temporary name and renamed into place,
//! so a reader never sees half of one.

use std::io;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

/// Bump when the digest layout changes; stale digests are rebuilt.
pub const DIGEST_SCHEMA: i32 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnRole {
    User,
    Assistant,
    Tool,
}

/// One merged turn of a conversation: consecutive text of one role is one turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DigestTurn {
    pub role: TurnRole,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// The start of what a tool call printed or returned, searchable like the rest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

/// Identifies the source transcript a digest was built from; a changed stamp means stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceStamp {
    pub len: i64,
    pub mtime_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DigestHeader {
    pub schema: i32,
    pub session_id: String,
    pub project_id: String,
    pub started_at: String,
    pub cwd: PathBuf,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub branch: Option<String>,
    /// What the session was about: the agent's title, else the person's first words with
    /// Nori's injected context removed.
    pub title: Option<String>,
    pub user_turns: i64,
    /// Every agent session this conversation has run in: the first, which a parent's transcript
    /// names its children by, and each one a resume continued it in. An agent that lists its own
    /// sessions lists them under these ids.
    pub acp_session_ids: Vec<String>,
    /// A delegated child: its transcript says so, or it opens with the delegation brief.
    pub delegated: bool,
    pub source: SourceStamp,
    pub indexed_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionDigest {
    pub header: DigestHeader,
    pub turns: Vec<DigestTurn>,
}

impl DigestTurn {
    /// Every string of the turn a search looks at.
    pub(super) fn texts(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.text.as_str())
            .chain(self.command.as_deref())
            .chain(self.output.as_deref())
    }
}

impl SessionDigest {
    /// Every string a search looks at, in the order of the conversation.
    pub fn texts(&self) -> impl Iterator<Item = &str> {
        self.header
            .branch
            .iter()
            .map(String::as_str)
            .chain(self.turns.iter().flat_map(DigestTurn::texts))
    }
}

/// The file of final turns.
pub(super) fn digest_path(root: &Path, project_id: &str, session_id: &str) -> PathBuf {
    root.join(project_id).join(format!("{session_id}.nidx"))
}

pub(super) fn bloom_path(root: &Path, project_id: &str, session_id: &str) -> PathBuf {
    root.join(project_id).join(format!("{session_id}.tri"))
}

pub(super) fn cursor_path(root: &Path, project_id: &str, session_id: &str) -> PathBuf {
    root.join(project_id).join(format!("{session_id}.nst"))
}

pub(super) fn header_path(root: &Path, project_id: &str, session_id: &str) -> PathBuf {
    root.join(project_id).join(format!("{session_id}.nhd"))
}

pub(super) fn read_header(path: &Path) -> io::Result<DigestHeader> {
    serde_json::from_slice(&std::fs::read(path)?).map_err(io::Error::other)
}

/// The final turns in the first `len` bytes of `path`.
pub(super) fn read_turns(path: &Path, len: i64) -> io::Result<Vec<DigestTurn>> {
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(u64::try_from(len).map_err(io::Error::other)?)
        .read_to_end(&mut bytes)?;
    if i64::try_from(bytes.len()).map_err(io::Error::other)? < len {
        return Err(io::Error::other(
            "digest turns are shorter than their cursor says",
        ));
    }
    bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).map_err(io::Error::other))
        .collect()
}

/// A session's digest: its header, its final turns and its unfinished ones. The cursor is read
/// first and names how much of the turns file is valid, so an update that lands meanwhile (it
/// appends before it replaces the cursor) is never half seen.
pub(super) fn read_digest(
    root: &Path,
    project_id: &str,
    session_id: &str,
) -> io::Result<SessionDigest> {
    let header = read_header(&header_path(root, project_id, session_id))?;
    let cursor = super::cursor::Cursor::read_any(&cursor_path(root, project_id, session_id))
        .ok_or_else(|| io::Error::other("digest has no cursor"))?;
    let mut turns = read_turns(
        &digest_path(root, project_id, session_id),
        cursor.turns_bytes,
    )?;
    turns.extend(cursor.extractor.tail_turns());
    Ok(SessionDigest { header, turns })
}

/// Append `turns` to the turns file after its first `keep` bytes, which are the valid ones: bytes
/// past them are from an update that did not finish. Returns the new length.
pub(super) fn append_turns(path: &Path, keep: i64, turns: &[DigestTurn]) -> io::Result<i64> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let keep = u64::try_from(keep).map_err(io::Error::other)?;
    if file.metadata()?.len() < keep {
        return Err(io::Error::other(
            "digest turns are shorter than their cursor says",
        ));
    }
    file.set_len(keep)?;
    let mut out = Vec::new();
    for turn in turns {
        serde_json::to_writer(&mut out, turn).map_err(io::Error::other)?;
        out.push(b'\n');
    }
    file.write_all(&out)?;
    i64::try_from(keep + u64::try_from(out.len()).map_err(io::Error::other)?)
        .map_err(io::Error::other)
}

/// Write `bytes` to `path` through a temporary file and a rename. There is no disk flush: the
/// index is derived from the transcripts, a digest that did not survive a power loss fails to
/// read and is rebuilt, and a flush costs 5 to 150 ms on macOS, twice per session indexed.
pub(super) fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("tmp");
    let mut file = std::fs::File::create(&temporary)?;
    file.write_all(bytes)?;
    std::fs::rename(temporary, path)
}
