//! Loading and committing text files, with the ledger checks around them.

use std::io::ErrorKind;
use std::path::Path;

use crate::error::ToolError;
use crate::fs::FileStat;
use crate::ledger::Freshness;
use crate::ledger::hash_bytes;
use crate::suggest::suggest;
use crate::util::cpu;
use crate::util::line_count;
use crate::workspace::Workspace;

/// A text file as read from disk.
#[derive(Debug)]
pub(crate) struct LoadedFile {
    pub text: String,
    /// SHA-256 of the file's bytes.
    pub hash: String,
    pub len: i64,
}

/// Whether the first bytes of a file say it is binary.
pub(crate) fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8000).any(|byte| *byte == 0)
}

impl Workspace {
    /// `stat` a path that should exist: a missing path is `NotFound` with suggestions.
    pub(crate) async fn stat_existing(&self, path: &Path) -> Result<FileStat, ToolError> {
        match self.fs().stat(path).await {
            Ok(stat) => Ok(stat),
            Err(error) if error.kind() == ErrorKind::NotFound => Err(ToolError::NotFound {
                path: path.to_path_buf(),
                suggestions: suggest(self.fs(), path).await,
            }),
            Err(error) => Err(ToolError::io("inspect", path, error)),
        }
    }

    /// `stat`, with a missing path as `None`.
    pub(crate) async fn stat_optional(&self, path: &Path) -> Result<Option<FileStat>, ToolError> {
        match self.fs().stat(path).await {
            Ok(stat) => Ok(Some(stat)),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(ToolError::io("inspect", path, error)),
        }
    }

    /// Read a UTF-8 text file already `stat`ed: within the size limit and not binary.
    pub(crate) async fn load_text_with(
        &self,
        path: &Path,
        stat: &FileStat,
    ) -> Result<LoadedFile, ToolError> {
        let limit = self.config().max_file_bytes;
        if stat.len > limit {
            return Err(ToolError::TooLarge {
                path: path.to_path_buf(),
                bytes: stat.len,
                limit,
            });
        }
        let bytes = self
            .fs()
            .read(path)
            .await
            .map_err(|error| ToolError::io("read", path, error))?;
        if looks_binary(&bytes) {
            return Err(ToolError::Binary {
                path: path.to_path_buf(),
                bytes: i64::try_from(bytes.len()).unwrap_or(i64::MAX),
            });
        }
        let len = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
        let (hash, text) = cpu(bytes.len(), move || {
            let hash = hash_bytes(&bytes);
            (hash, String::from_utf8(bytes))
        })
        .await?;
        let text = text.map_err(|_| ToolError::NotUtf8 {
            path: path.to_path_buf(),
        })?;
        Ok(LoadedFile { text, hash, len })
    }

    /// Refuse to change a file the model has not read, or that changed since it did.
    pub(crate) fn require_fresh(&self, path: &Path, loaded: &LoadedFile) -> Result<(), ToolError> {
        if !self.config().require_read_before_edit || loaded.len == 0 {
            return Ok(());
        }
        match self.ledger().check(path, &loaded.hash) {
            Freshness::Fresh => Ok(()),
            Freshness::NeverRead => Err(ToolError::NotRead {
                path: path.to_path_buf(),
            }),
            Freshness::ChangedOnDisk => Err(ToolError::ChangedSinceRead {
                path: path.to_path_buf(),
            }),
        }
    }

    /// Write a file and record the write in the ledger.
    pub(crate) async fn write_text(&self, path: &Path, text: &str) -> Result<(), ToolError> {
        self.fs()
            .write(path, text.as_bytes())
            .await
            .map_err(|error| ToolError::io("write", path, error))?;
        let modified = self
            .fs()
            .stat(path)
            .await
            .ok()
            .and_then(|stat| stat.modified);
        let bytes = text.as_bytes().to_vec();
        let hash = cpu(bytes.len(), move || hash_bytes(&bytes)).await?;
        self.ledger()
            .record_write(path, &hash, modified, line_count(text));
        Ok(())
    }
}
