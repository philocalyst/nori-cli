//! The session bundle on the wire: a manifest, then the files, each with its length, so a bundle
//! streams through a pipe or an ssh connection and is read back byte for byte.
//!
//! ```text
//! nori-session-bundle 1
//! {manifest JSON}
//! file root|child SESSION_ID jsonl|pkl LENGTH
//! LENGTH bytes
//! ...
//! end
//! ```

use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

/// The format this Nori writes and reads.
pub const BUNDLE_VERSION: i64 = 1;
const MAGIC: &str = "nori-session-bundle";

/// What a bundle says about the session, before its files.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// The conversation ID: the same on every machine the session moves to.
    pub conversation_id: String,
    pub project: BundledProject,
    pub origin: BundledOrigin,
    /// The agent slug, account included (`codex@work`).
    pub agent: Option<String>,
    /// Where the working tree stood.
    pub git: Option<BundledGit>,
    /// When the bundle was made (ISO 8601).
    pub exported_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BundledProject {
    /// The project id on the origin: a hash of the git remote when there is one, so the same
    /// on every machine with a clone of it.
    pub id: String,
    pub name: String,
    pub git_remote: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BundledOrigin {
    pub hostname: String,
    /// What the user's machines call the origin (its Rex label), so a job left running there
    /// is adopted from the target as a job of that machine.
    #[serde(default)]
    pub machine: Option<String>,
    pub cwd: PathBuf,
    pub home: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BundledGit {
    pub branch: Option<String>,
    pub head: Option<String>,
}

/// Whose file it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The conversation itself.
    Root,
    /// A delegated child's.
    Child,
}

/// Which file of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// The transcript (`.jsonl`).
    Transcript,
    /// The conversation journal (`.pkl`).
    Journal,
}

impl FileKind {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Transcript => "jsonl",
            Self::Journal => "pkl",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleFile {
    pub role: Role,
    pub session_id: String,
    pub kind: FileKind,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionBundle {
    pub manifest: Manifest,
    pub files: Vec<BundleFile>,
}

#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    #[error("this is not a Nori session bundle")]
    NotABundle,
    #[error("the bundle is version {0}; this Nori reads version {BUNDLE_VERSION}")]
    Version(i64),
    #[error("the bundle is damaged: {0}")]
    Damaged(String),
}

/// A session id names a file: nothing but letters, digits, `-` and `_`.
pub(super) fn is_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

impl SessionBundle {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = format!("{MAGIC} {BUNDLE_VERSION}\n").into_bytes();
        out.extend(serde_json::to_vec(&self.manifest).unwrap_or_default());
        out.push(b'\n');
        for file in &self.files {
            let role = match file.role {
                Role::Root => "root",
                Role::Child => "child",
            };
            out.extend(
                format!(
                    "file {role} {} {} {}\n",
                    file.session_id,
                    file.kind.extension(),
                    file.bytes.len()
                )
                .into_bytes(),
            );
            out.extend(&file.bytes);
        }
        out.extend(b"end\n");
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, BundleError> {
        let mut rest = bytes;
        let line = |rest: &mut &[u8]| -> Option<String> {
            let newline = rest.iter().position(|&byte| byte == b'\n')?;
            let text = String::from_utf8_lossy(&rest[..newline]).into_owned();
            *rest = &rest[newline + 1..];
            Some(text)
        };
        let header = line(&mut rest).ok_or(BundleError::NotABundle)?;
        let version = header
            .strip_prefix(MAGIC)
            .and_then(|version| version.trim().parse::<i64>().ok())
            .ok_or(BundleError::NotABundle)?;
        if version != BUNDLE_VERSION {
            return Err(BundleError::Version(version));
        }
        let manifest =
            line(&mut rest).ok_or_else(|| BundleError::Damaged("no manifest".to_owned()))?;
        let manifest: Manifest = serde_json::from_str(&manifest)
            .map_err(|error| BundleError::Damaged(format!("manifest: {error}")))?;
        let mut files = Vec::new();
        loop {
            let entry =
                line(&mut rest).ok_or_else(|| BundleError::Damaged("it ends early".to_owned()))?;
            if entry == "end" {
                break;
            }
            let fields: Vec<&str> = entry.split(' ').collect();
            let ["file", role, session_id, extension, length] = fields.as_slice() else {
                return Err(BundleError::Damaged(format!("unexpected `{entry}`")));
            };
            let role = match *role {
                "root" => Role::Root,
                "child" => Role::Child,
                other => return Err(BundleError::Damaged(format!("unknown role `{other}`"))),
            };
            let kind = match *extension {
                "jsonl" => FileKind::Transcript,
                "pkl" => FileKind::Journal,
                other => return Err(BundleError::Damaged(format!("unknown file `{other}`"))),
            };
            if !is_session_id(session_id) {
                return Err(BundleError::Damaged(format!(
                    "`{session_id}` is not a session id"
                )));
            }
            let length: usize = length
                .parse()
                .map_err(|_| BundleError::Damaged(format!("bad length in `{entry}`")))?;
            if rest.len() < length {
                return Err(BundleError::Damaged("a file is cut short".to_owned()));
            }
            let (body, after) = rest.split_at(length);
            rest = after;
            files.push(BundleFile {
                role,
                session_id: (*session_id).to_owned(),
                kind,
                bytes: body.to_vec(),
            });
        }
        Ok(Self { manifest, files })
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn a_bundle_reads_back_byte_for_byte() {
        let bundle = SessionBundle {
            manifest: Manifest {
                conversation_id: "c0ffee-1".to_owned(),
                project: BundledProject {
                    id: "abc".to_owned(),
                    name: "app".to_owned(),
                    git_remote: Some("git@example.com:me/app.git".to_owned()),
                },
                origin: BundledOrigin {
                    hostname: "laptop".to_owned(),
                    machine: Some("laptop".to_owned()),
                    cwd: PathBuf::from("/Users/me/src/app"),
                    home: Some(PathBuf::from("/Users/me")),
                },
                agent: Some("claude-code".to_owned()),
                git: Some(BundledGit {
                    branch: Some("main".to_owned()),
                    head: Some("deadbeef".to_owned()),
                }),
                exported_at: "2026-11-19T00:00:00Z".to_owned(),
            },
            files: vec![
                BundleFile {
                    role: Role::Root,
                    session_id: "c0ffee-1".to_owned(),
                    kind: FileKind::Transcript,
                    bytes: b"{\"line\":1}\nend\nfile root x jsonl 3\n".to_vec(),
                },
                BundleFile {
                    role: Role::Child,
                    session_id: "child-2".to_owned(),
                    kind: FileKind::Journal,
                    bytes: vec![0, 159, 146, 150, b'\n'],
                },
            ],
        };
        assert_eq!(
            SessionBundle::decode(&bundle.encode()).ok(),
            Some(bundle.clone())
        );
        let mut damaged = bundle.encode();
        damaged.truncate(damaged.len() - 10);
        assert!(SessionBundle::decode(&damaged).is_err());
        assert!(matches!(
            SessionBundle::decode(b"nori-session-bundle 9\n{}\n"),
            Err(BundleError::Version(9))
        ));
    }
}
