//! The filesystem the tools run against.
//!
//! [`ToolFs`] is the seam a remote machine plugs into: every tool reads and writes through it, so
//! running the tools on another machine means implementing this trait over that machine's
//! transport. [`LocalFs`] is the tokio implementation for this machine.

use std::fmt::Debug;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;

use async_trait::async_trait;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    File,
    Dir,
    Other,
}

/// `stat` of a path, following symlinks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStat {
    pub kind: FileKind,
    pub len: i64,
    pub modified: Option<SystemTime>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub kind: FileKind,
}

/// Asynchronous file operations the tools need. Implementations must not block the runtime.
#[async_trait]
pub trait ToolFs: Send + Sync + Debug {
    async fn stat(&self, path: &Path) -> io::Result<FileStat>;
    async fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    /// Write the whole file, creating missing parent directories. Should be atomic where the
    /// machine allows it, and should keep the existing file's permissions.
    async fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()>;
    async fn remove_file(&self, path: &Path) -> io::Result<()>;
    async fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>>;
}

/// The local machine, through tokio's filesystem.
#[derive(Debug, Default, Clone, Copy)]
pub struct LocalFs;

static TEMP_COUNTER: AtomicI64 = AtomicI64::new(0);

fn kind_of(file_type: std::fs::FileType) -> FileKind {
    if file_type.is_file() {
        FileKind::File
    } else if file_type.is_dir() {
        FileKind::Dir
    } else {
        FileKind::Other
    }
}

#[async_trait]
impl ToolFs for LocalFs {
    async fn stat(&self, path: &Path) -> io::Result<FileStat> {
        let meta = tokio::fs::metadata(path).await?;
        Ok(FileStat {
            kind: kind_of(meta.file_type()),
            len: i64::try_from(meta.len()).unwrap_or(i64::MAX),
            modified: meta.modified().ok(),
        })
    }

    async fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        tokio::fs::read(path).await
    }

    async fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        // Write through a symlink to its target instead of replacing the link.
        let target = match tokio::fs::symlink_metadata(path).await {
            Ok(meta) if meta.file_type().is_symlink() => tokio::fs::canonicalize(path).await?,
            Ok(_) | Err(_) => path.to_path_buf(),
        };
        if let Some(parent) = target
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            tokio::fs::create_dir_all(parent).await?;
        }
        let existing_permissions = tokio::fs::metadata(&target)
            .await
            .ok()
            .map(|meta| meta.permissions());

        let file_name = target
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp: PathBuf = target.with_file_name(format!(
            ".{file_name}.nori-{}-{counter}",
            std::process::id()
        ));

        let result = async {
            tokio::fs::write(&temp, contents).await?;
            if let Some(permissions) = existing_permissions {
                tokio::fs::set_permissions(&temp, permissions).await?;
            }
            tokio::fs::rename(&temp, &target).await
        }
        .await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(&temp).await;
        }
        result
    }

    async fn remove_file(&self, path: &Path) -> io::Result<()> {
        tokio::fs::remove_file(path).await
    }

    async fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        let mut entries = tokio::fs::read_dir(path).await?;
        let mut out = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let kind = match tokio::fs::metadata(entry.path()).await {
                Ok(meta) => kind_of(meta.file_type()),
                Err(_) => FileKind::Other,
            };
            out.push(DirEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                kind,
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }
}
