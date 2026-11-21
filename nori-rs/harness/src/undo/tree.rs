//! Shadow-git snapshots of a project's working tree.
//!
//! A [`ShadowTree`] keeps a git directory of its own under `$NORI_HOME/snapshots/<hash of the
//! project's root>/` (never inside the project) and uses the project's directory as its work tree.
//! [`ShadowTree::capture`] stages every file the project's ignore rules leave visible and writes
//! the result as a git tree object, so it records what a shell command did as faithfully as what
//! an editor did. Nothing here touches the real repository's index or refs.
//!
//! Every git command is a `tokio` process, so nothing waits on the event loop. The shadow index
//! stays between captures, which is what makes the second capture of a large tree cheap: git only
//! re-stats the files.
//!
//! The work tree is the project's git root, not the session's directory: a `.gitignore` above the
//! directory only applies to a work tree that contains it, and an agent that edits a sibling
//! directory is still undone.

mod restore;
#[cfg(test)]
mod tests;

use std::path::Path;
use std::path::PathBuf;
use std::process::Output;
use std::process::Stdio;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::debug;
use tracing::warn;

/// What makes a tree too big to snapshot on every turn.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    /// How long the first capture may take before snapshots are turned off for the session. It
    /// writes every file of the project once, in the background, and later captures re-stat only
    /// what changed.
    pub first_capture: Duration,
    /// How many files the index may hold.
    pub max_entries: i64,
}

impl Limits {
    pub(crate) const DEFAULT: Self = Self {
        first_capture: Duration::from_secs(10),
        max_entries: 200_000,
    };
}

/// The id of a git tree object that records every visible file of the project at one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TreeId(String);

#[derive(Debug, thiserror::Error)]
pub(crate) enum SnapshotError {
    /// `git` is missing or the directory is not inside a git work tree.
    #[error("git is not available here or the directory is not in a git work tree")]
    NoGit,
    #[error("git {step} failed: {stderr}")]
    Failed { step: &'static str, stderr: String },
    /// The tree is too big to capture on every turn; snapshots are off for the session.
    #[error("{reason}")]
    TooLarge { reason: String },
}

/// How a path differs between two trees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChangeKind {
    Added,
    Modified,
    Deleted,
    TypeChanged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TreeChange {
    pub kind: ChangeKind,
    /// Relative to the work tree.
    pub path: String,
}

#[derive(Debug)]
pub(crate) struct ShadowTree {
    git_dir: PathBuf,
    work_tree: PathBuf,
    index: PathBuf,
    limits: Limits,
    /// One command at a time uses the shadow index.
    serial: tokio::sync::Mutex<()>,
    attempted: AtomicBool,
    disabled: AtomicBool,
}

const NULL_DEVICE: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };
const STALE_INDEX: Duration = Duration::from_secs(2 * 24 * 60 * 60);
const GC_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

impl ShadowTree {
    /// The shadow tree of the git work tree that contains `cwd`, created under `nori_home` if it
    /// does not exist yet.
    pub(crate) async fn open(
        cwd: &Path,
        nori_home: &Path,
        limits: Limits,
    ) -> Result<Self, SnapshotError> {
        // This only asks the real repository where it is; the shadow repository does the rest.
        let found = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args([
                "rev-parse",
                "--path-format=absolute",
                "--show-toplevel",
                "--git-path",
                "info/exclude",
            ])
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|_| SnapshotError::NoGit)?;
        if !found.status.success() {
            return Err(SnapshotError::NoGit);
        }
        let found = String::from_utf8_lossy(&found.stdout).into_owned();
        let mut lines = found.lines();
        let (Some(top), Some(exclude)) = (lines.next(), lines.next()) else {
            return Err(SnapshotError::NoGit);
        };
        let work_tree = tokio::fs::canonicalize(top)
            .await
            .map_err(|_| SnapshotError::NoGit)?;

        // A stable name for the project: FNV-1a over its canonical root.
        let name = work_tree
            .as_os_str()
            .as_encoded_bytes()
            .iter()
            .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
                (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
            });
        let git_dir = nori_home.join("snapshots").join(format!("{name:016x}"));
        let tree = Self {
            index: git_dir.join(format!("index-{}", uuid::Uuid::new_v4())),
            git_dir,
            work_tree,
            limits,
            serial: tokio::sync::Mutex::new(()),
            attempted: AtomicBool::new(false),
            disabled: AtomicBool::new(false),
        };
        tree.prepare(Path::new(exclude)).await?;
        Ok(tree)
    }

    async fn prepare(&self, real_exclude: &Path) -> Result<(), SnapshotError> {
        let failed = |step, error: std::io::Error| SnapshotError::Failed {
            step,
            stderr: error.to_string(),
        };
        let info = self.git_dir.join("info");
        tokio::fs::create_dir_all(&info)
            .await
            .map_err(|error| failed("prepare", error))?;
        if !tokio::fs::try_exists(self.git_dir.join("HEAD"))
            .await
            .unwrap_or(false)
        {
            self.run(
                "init",
                &[
                    "-c",
                    "init.defaultBranch=main",
                    "init",
                    "--quiet",
                    "--template=",
                ],
                &self.index,
                None,
            )
            .await?;
        }
        // The real repository's private ignore rules apply to the shadow work tree too.
        let rules = tokio::fs::read(real_exclude).await.unwrap_or_default();
        tokio::fs::write(info.join("exclude"), rules)
            .await
            .map_err(|error| failed("prepare", error))
    }

    fn command(&self, index: &Path) -> Command {
        let mut command = Command::new("git");
        command
            .current_dir(&self.work_tree)
            .args([
                "-c",
                "core.autocrlf=false",
                "-c",
                "core.quotepath=false",
                "-c",
                "core.safecrlf=false",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "advice.addEmbeddedRepo=false",
                "-c",
                "gc.auto=0",
                // Snapshots are disposable: skip the fsync of every object and the zlib work.
                "-c",
                "core.fsync=none",
                "-c",
                "core.compression=0",
                "-c",
                "core.looseCompression=0",
            ])
            .env("GIT_DIR", &self.git_dir)
            .env("GIT_WORK_TREE", &self.work_tree)
            .env("GIT_INDEX_FILE", index)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", NULL_DEVICE)
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("LC_ALL", "C")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_NAMESPACE")
            .kill_on_drop(true);
        command
    }

    /// Run one shadow git command to its end, whatever its exit status.
    async fn run_raw(
        &self,
        step: &'static str,
        args: &[&str],
        index: &Path,
        stdin: Option<Vec<u8>>,
    ) -> Result<Output, SnapshotError> {
        let mut command = self.command(index);
        command
            .args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => SnapshotError::NoGit,
            _ => SnapshotError::Failed {
                step,
                stderr: error.to_string(),
            },
        })?;
        let pipe = child.stdin.take();
        let feed = async {
            if let (Some(bytes), Some(mut pipe)) = (stdin, pipe) {
                // A command that exits early closes the pipe; its status says why.
                let _ = pipe.write_all(&bytes).await;
            }
        };
        let (output, ()) = tokio::join!(child.wait_with_output(), feed);
        output.map_err(|error| SnapshotError::Failed {
            step,
            stderr: error.to_string(),
        })
    }

    async fn run(
        &self,
        step: &'static str,
        args: &[&str],
        index: &Path,
        stdin: Option<Vec<u8>>,
    ) -> Result<Vec<u8>, SnapshotError> {
        let output = self.run_raw(step, args, index, stdin).await?;
        if output.status.success() {
            Ok(output.stdout)
        } else {
            Err(SnapshotError::Failed {
                step,
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            })
        }
    }

    /// Record the work tree as it is now. The first capture decides whether the tree is small
    /// enough to snapshot at all; once it is not, every capture says so without running git.
    pub(crate) async fn capture(&self) -> Result<TreeId, SnapshotError> {
        let _serial = self.serial.lock().await;
        if self.disabled.load(Ordering::Relaxed) {
            return Err(SnapshotError::TooLarge {
                reason: "this project is too big to snapshot".to_string(),
            });
        }
        let started = Instant::now();
        let first = !self.attempted.swap(true, Ordering::Relaxed);
        let tree = if first {
            match tokio::time::timeout(self.limits.first_capture, self.stage_and_write(true)).await
            {
                Ok(tree) => tree?,
                Err(_) => return Err(self.disable("the first snapshot took too long")),
            }
        } else {
            self.stage_and_write(false).await?
        };
        debug!(elapsed_ms = started.elapsed().as_millis() as i64, first, tree = %tree.0, "captured a tree snapshot");
        if first {
            let listing = self
                .run("ls-files", &["ls-files", "-z"], &self.index, None)
                .await?;
            let entries = memchr::memchr_iter(0, &listing).count() as i64;
            if entries > self.limits.max_entries {
                return Err(self.disable("the project has too many files"));
            }
        }
        Ok(tree)
    }

    fn disable(&self, reason: &str) -> SnapshotError {
        self.disabled.store(true, Ordering::Relaxed);
        SnapshotError::TooLarge {
            reason: reason.to_string(),
        }
    }

    /// Stage everything and write the tree. `bulk` writes every new object into one packfile
    /// (a `core.bigFileThreshold` of one byte makes git stream each file into a single pack)
    /// instead of one loose file per object: a first capture of a project writes thousands, and
    /// creating them one by one costs several times what the pack does.
    async fn stage_and_write(&self, bulk: bool) -> Result<TreeId, SnapshotError> {
        // A capture dropped halfway (a timeout) leaves its lock behind; this index is this
        // tree's alone and the serial lock is held, so any lock file is stale.
        let mut lock = self.index.clone().into_os_string();
        lock.push(".lock");
        let _ = tokio::fs::remove_file(lock).await;
        let threshold = if bulk { "1" } else { "536870912" };
        let staged = self
            .run_raw(
                "add",
                &[
                    "-c",
                    &format!("core.bigFileThreshold={threshold}"),
                    "add",
                    "-A",
                    "--ignore-errors",
                ],
                &self.index,
                None,
            )
            .await?;
        // A partial tree cannot safely represent the working directory for undo.
        if !staged.status.success() {
            return Err(SnapshotError::Failed {
                step: "add",
                stderr: String::from_utf8_lossy(&staged.stderr).trim().to_string(),
            });
        }
        let written = self
            .run("write-tree", &["write-tree"], &self.index, None)
            .await?;
        Ok(TreeId(String::from_utf8_lossy(&written).trim().to_string()))
    }

    /// The paths that differ between two trees.
    pub(crate) async fn changes(
        &self,
        from: &TreeId,
        to: &TreeId,
    ) -> Result<Vec<TreeChange>, SnapshotError> {
        let output = self
            .run(
                "diff-tree",
                &[
                    "diff-tree",
                    "-r",
                    "-z",
                    "--name-status",
                    "--no-renames",
                    "--no-commit-id",
                    &from.0,
                    &to.0,
                ],
                &self.index,
                None,
            )
            .await?;
        let text = String::from_utf8_lossy(&output);
        let mut fields = text.split('\0').filter(|field| !field.is_empty());
        let mut changes = Vec::new();
        while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
            let kind = match status {
                "A" => ChangeKind::Added,
                "M" => ChangeKind::Modified,
                "D" => ChangeKind::Deleted,
                "T" => ChangeKind::TypeChanged,
                other => {
                    return Err(SnapshotError::Failed {
                        step: "diff-tree",
                        stderr: format!("unexpected status {other} for {path}"),
                    });
                }
            };
            changes.push(TreeChange {
                kind,
                path: path.to_string(),
            });
        }
        Ok(changes)
    }

    /// Prune what snapshots left behind: shadow objects older than 7 days, and the indexes of
    /// sessions long gone. Runs at most once a day per shadow directory, and never on a hot path.
    pub(crate) async fn maintain(&self) {
        let marker = self.git_dir.join("nori-last-gc");
        let due = match tokio::fs::metadata(&marker)
            .await
            .and_then(|metadata| metadata.modified())
        {
            Ok(modified) => SystemTime::now()
                .duration_since(modified)
                .is_ok_and(|age| age >= GC_INTERVAL),
            Err(_) => true,
        };
        if !due || tokio::fs::write(&marker, b"").await.is_err() {
            return;
        }
        if let Ok(mut entries) = tokio::fs::read_dir(&self.git_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let stale = entry.file_name().to_string_lossy().starts_with("index-")
                    && entry
                        .metadata()
                        .await
                        .and_then(|metadata| metadata.modified())
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|age| age > STALE_INDEX);
                // A live session whose index is removed only starts its next capture cold.
                if stale && entry.path() != self.index {
                    let _ = tokio::fs::remove_file(entry.path()).await;
                }
            }
        }
        match self
            .run_raw(
                "gc",
                &["gc", "--quiet", "--prune=7.days.ago"],
                &self.index,
                None,
            )
            .await
        {
            Ok(output) if output.status.success() => debug!("pruned old snapshot objects"),
            Ok(output) => warn!(
                "snapshot gc failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            Err(error) => warn!("snapshot gc failed: {error}"),
        }
    }
}
