//! Nori's file tools on another machine: [`nori_tools::ToolFs`] over the machine's own
//! connection ([`nori_machines::MachineFs`]), so read, write, edit and patch work there the way
//! `bash` does (see `docs/machines.md`).

use std::io;
use std::path::Path;

use async_trait::async_trait;
use nori_machines::MachineFs;
use nori_machines::PathKind;
use nori_tools::DirEntry;
use nori_tools::FileKind;
use nori_tools::FileStat;

#[derive(Debug, Clone)]
pub(crate) struct MachineToolFs(pub(crate) MachineFs);

fn kind(kind: PathKind) -> FileKind {
    match kind {
        PathKind::File => FileKind::File,
        PathKind::Dir => FileKind::Dir,
        PathKind::Other => FileKind::Other,
    }
}

fn path_text(path: &Path) -> String {
    path.display().to_string()
}

#[async_trait]
impl nori_tools::ToolFs for MachineToolFs {
    async fn stat(&self, path: &Path) -> io::Result<FileStat> {
        let stat = self.0.stat(&path_text(path)).await?;
        Ok(FileStat {
            kind: kind(stat.kind),
            len: stat.len,
            modified: stat.modified,
        })
    }

    async fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.0.read(&path_text(path)).await
    }

    async fn write(&self, path: &Path, contents: &[u8]) -> io::Result<()> {
        self.0.write(&path_text(path), contents).await
    }

    async fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.0.remove_file(&path_text(path)).await
    }

    async fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        Ok(self
            .0
            .read_dir(&path_text(path))
            .await?
            .into_iter()
            .map(|(name, entry)| DirEntry {
                name,
                kind: kind(entry),
            })
            .collect())
    }
}
