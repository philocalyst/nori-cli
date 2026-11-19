//! The on-disk digest of one session: what a search and a preview need, without the protocol
//! traffic that makes up most of a raw transcript.
//!
//! `index/v1/<project>/<session>.nidx` is JSON lines: a [`DigestHeader`], then one
//! [`DigestTurn`] per line. Next to it, `<session>.tri` is the trigram bloom filter
//! ([`super::bloom::Bloom`]). Both are written to a temporary name and renamed into place, so
//! a reader never sees half of either.

use std::io;
use std::io::BufRead;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

/// Bump when the digest layout changes; stale digests are rebuilt.
pub const DIGEST_SCHEMA: i32 = 4;

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
    /// First line of the first user message.
    pub title: Option<String>,
    pub user_turns: i64,
    /// The agent's id for this session, which a parent's transcript names its children by.
    pub acp_session_id: Option<String>,
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

impl SessionDigest {
    /// Every string a search looks at, in the order of the conversation.
    pub fn texts(&self) -> impl Iterator<Item = &str> {
        self.header
            .branch
            .iter()
            .map(String::as_str)
            .chain(self.turns.iter().flat_map(|turn| {
                std::iter::once(turn.text.as_str())
                    .chain(turn.command.as_deref())
                    .chain(turn.output.as_deref())
            }))
    }
}

pub(super) fn digest_path(root: &Path, project_id: &str, session_id: &str) -> PathBuf {
    root.join(project_id).join(format!("{session_id}.nidx"))
}

pub(super) fn bloom_path(root: &Path, project_id: &str, session_id: &str) -> PathBuf {
    root.join(project_id).join(format!("{session_id}.tri"))
}

/// Read only the header line.
pub(super) fn read_header(path: &Path) -> io::Result<DigestHeader> {
    let file = std::fs::File::open(path)?;
    let mut line = String::new();
    io::BufReader::new(file).read_line(&mut line)?;
    serde_json::from_str(&line).map_err(io::Error::other)
}

pub(super) fn read_digest(path: &Path) -> io::Result<SessionDigest> {
    let file = std::fs::File::open(path)?;
    let mut lines = io::BufReader::new(file).lines();
    let header: DigestHeader = match lines.next() {
        Some(line) => serde_json::from_str(&line?).map_err(io::Error::other)?,
        None => return Err(io::Error::other("empty digest")),
    };
    let mut turns = Vec::new();
    for line in lines {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        turns.push(serde_json::from_str(&line).map_err(io::Error::other)?);
    }
    Ok(SessionDigest { header, turns })
}

/// Write `bytes` to `path` through a temporary file and a rename.
pub(super) fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("tmp");
    let mut file = std::fs::File::create(&temporary)?;
    file.write_all(bytes)?;
    file.sync_data()?;
    std::fs::rename(temporary, path)
}

pub(super) fn encode(digest: &SessionDigest) -> io::Result<Vec<u8>> {
    let mut out = serde_json::to_vec(&digest.header).map_err(io::Error::other)?;
    out.push(b'\n');
    for turn in &digest.turns {
        serde_json::to_writer(&mut out, turn).map_err(io::Error::other)?;
        out.push(b'\n');
    }
    Ok(out)
}
