//! Diagnostics: the plain types, the per-file store the servers publish into, and the
//! "wait for the server to speak after an edit" logic.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::watch;
use tokio::time::Instant;

use crate::types::Range;

/// Ordered so that sorting puts errors first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
    Information,
    Hint,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Information => "info",
            Self::Hint => "hint",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub severity: Severity,
    pub range: Range,
    pub message: String,
    pub source: Option<String>,
    pub code: Option<String>,
    /// The language server that published it.
    pub server: String,
}

impl Diagnostic {
    /// What identifies a diagnostic across publishes, for "is this error new".
    fn key(&self) -> (i32, String) {
        (self.range.line, self.message.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiagnostics {
    pub path: PathBuf,
    pub diagnostics: Vec<Diagnostic>,
}

/// How far the report can be trusted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "detail", rename_all = "snake_case")]
pub enum ReportStatus {
    /// A server published after the change and then went quiet.
    Fresh,
    /// No server published within the wait; the diagnostics are the last known ones.
    Stale,
    /// No language server serves this file.
    NoServer,
    /// The server for this file is down; the string says why.
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticsReport {
    pub path: PathBuf,
    /// Diagnostics of the changed file, errors first.
    pub file: Vec<Diagnostic>,
    /// Errors in other files that were not there before the change.
    pub project: Vec<FileDiagnostics>,
    pub servers: Vec<String>,
    pub status: ReportStatus,
}

impl DiagnosticsReport {
    pub fn without_server(path: PathBuf, status: ReportStatus) -> Self {
        Self {
            path,
            file: Vec::new(),
            project: Vec::new(),
            servers: Vec::new(),
            status,
        }
    }

    pub fn error_count(&self) -> i32 {
        count(&self.file, Severity::Error)
    }
}

pub fn count(diagnostics: &[Diagnostic], severity: Severity) -> i32 {
    let total = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == severity)
        .count();
    i32::try_from(total).unwrap_or(i32::MAX)
}

/// Error keys of every file but one, taken when a change is announced.
#[derive(Debug, Clone, Default)]
pub struct ProjectBaseline {
    errors: HashMap<PathBuf, HashSet<(i32, String)>>,
}

#[derive(Default)]
struct FileEntry {
    by_server: HashMap<i64, Vec<Diagnostic>>,
    last_publish: i64,
}

#[derive(Default)]
struct State {
    files: HashMap<PathBuf, FileEntry>,
    seq: i64,
}

struct Shared {
    state: Mutex<State>,
    seq: watch::Sender<i64>,
}

/// The diagnostics of every file any server published, merged across servers. Cheap to clone; all
/// operations are short and synchronous, and nothing is held across an await.
#[derive(Clone)]
pub struct DiagnosticStore {
    shared: Arc<Shared>,
}

impl Default for DiagnosticStore {
    fn default() -> Self {
        let (seq, _) = watch::channel(0);
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State::default()),
                seq,
            }),
        }
    }
}

impl DiagnosticStore {
    /// Replaces what `server` knows about `path` (an empty list clears it) and wakes subscribers.
    pub fn publish(&self, server: i64, path: PathBuf, diagnostics: Vec<Diagnostic>) {
        let seq = {
            let mut state = self.shared.state.lock();
            state.seq += 1;
            let seq = state.seq;
            let entry = state.files.entry(path).or_default();
            entry.last_publish = seq;
            entry.by_server.insert(server, diagnostics);
            seq
        };
        self.shared.seq.send_replace(seq);
    }

    /// Forgets everything `server` published (it crashed or was stopped).
    pub fn clear_server(&self, server: i64) {
        let seq = {
            let mut state = self.shared.state.lock();
            state.seq += 1;
            for entry in state.files.values_mut() {
                entry.by_server.remove(&server);
            }
            state.files.retain(|_, entry| !entry.by_server.is_empty());
            state.seq
        };
        self.shared.seq.send_replace(seq);
    }

    /// A counter that grows on every publish; the UI re-reads on change.
    pub fn subscribe(&self) -> watch::Receiver<i64> {
        self.shared.seq.subscribe()
    }

    pub fn seq(&self) -> i64 {
        self.shared.state.lock().seq
    }

    /// The sequence number of the last publish for `path` (0: never).
    pub fn last_publish(&self, path: &Path) -> i64 {
        self.shared
            .state
            .lock()
            .files
            .get(path)
            .map_or(0, |entry| entry.last_publish)
    }

    /// Diagnostics of one file, errors first, then by position.
    pub fn file(&self, path: &Path) -> Vec<Diagnostic> {
        let state = self.shared.state.lock();
        let mut all: Vec<Diagnostic> = state
            .files
            .get(path)
            .map(|entry| entry.by_server.values().flatten().cloned().collect())
            .unwrap_or_default();
        sort(&mut all);
        all
    }

    /// Every file with at least one diagnostic of `severity` or worse, sorted by path.
    pub fn files_with(&self, severity: Severity) -> Vec<FileDiagnostics> {
        let state = self.shared.state.lock();
        let mut files: Vec<FileDiagnostics> = state
            .files
            .iter()
            .filter_map(|(path, entry)| {
                let mut diagnostics: Vec<Diagnostic> = entry
                    .by_server
                    .values()
                    .flatten()
                    .filter(|diagnostic| diagnostic.severity <= severity)
                    .cloned()
                    .collect();
                sort(&mut diagnostics);
                (!diagnostics.is_empty()).then(|| FileDiagnostics {
                    path: path.clone(),
                    diagnostics,
                })
            })
            .collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        files
    }

    /// The errors of every file except `path`, to compare against after a change.
    pub fn baseline_except(&self, path: &Path) -> ProjectBaseline {
        let state = self.shared.state.lock();
        let errors = state
            .files
            .iter()
            .filter(|(other, _)| other.as_path() != path)
            .map(|(other, entry)| {
                let keys = entry
                    .by_server
                    .values()
                    .flatten()
                    .filter(|diagnostic| diagnostic.severity == Severity::Error)
                    .map(Diagnostic::key)
                    .collect();
                (other.clone(), keys)
            })
            .collect();
        ProjectBaseline { errors }
    }

    /// Errors in files other than `path` that the baseline did not have.
    pub fn new_errors_since(
        &self,
        baseline: &ProjectBaseline,
        path: &Path,
    ) -> Vec<FileDiagnostics> {
        let mut out = Vec::new();
        for file in self.files_with(Severity::Error) {
            if file.path == path {
                continue;
            }
            let known = baseline.errors.get(&file.path);
            let fresh: Vec<Diagnostic> = file
                .diagnostics
                .into_iter()
                .filter(|diagnostic| diagnostic.severity == Severity::Error)
                .filter(|diagnostic| known.is_none_or(|keys| !keys.contains(&diagnostic.key())))
                .collect();
            if !fresh.is_empty() {
                out.push(FileDiagnostics {
                    path: file.path,
                    diagnostics: fresh,
                });
            }
        }
        out
    }

    /// Waits for a server to publish for `path` after `baseline_seq`: up to `first` for the first
    /// publish, then until `settle` passes without another (at most `cap` in all). Returns whether
    /// anything was published. Cancel-safe: it only reads.
    pub async fn wait_for_publish(
        &self,
        path: &Path,
        baseline_seq: i64,
        first: Duration,
        settle: Duration,
        cap: Duration,
    ) -> bool {
        let mut seq = self.subscribe();
        let first_deadline = Instant::now() + first;
        while self.last_publish(path) <= baseline_seq {
            match tokio::time::timeout_at(first_deadline, seq.changed()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) | Err(_) => return false,
            }
        }
        let cap_deadline = Instant::now() + cap;
        loop {
            let seen = self.last_publish(path);
            let quiet = Instant::now() + settle;
            let deadline = quiet.min(cap_deadline);
            loop {
                match tokio::time::timeout_at(deadline, seq.changed()).await {
                    Ok(Ok(())) if self.last_publish(path) != seen => break,
                    Ok(Ok(())) => {}
                    Ok(Err(_)) | Err(_) => return true,
                }
            }
            if Instant::now() >= cap_deadline {
                return true;
            }
        }
    }
}

fn sort(diagnostics: &mut [Diagnostic]) {
    diagnostics.sort_by(|a, b| {
        (a.severity, a.range.line, a.range.column).cmp(&(b.severity, b.range.line, b.range.column))
    });
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    pub(crate) fn diag(severity: Severity, line: i32, message: &str) -> Diagnostic {
        Diagnostic {
            severity,
            range: Range {
                line,
                column: 1,
                end_line: line,
                end_column: 2,
            },
            message: message.to_owned(),
            source: Some("fake".to_owned()),
            code: None,
            server: "fake-ls".to_owned(),
        }
    }

    #[test]
    fn merges_servers_and_sorts_errors_first() {
        let store = DiagnosticStore::default();
        let path = PathBuf::from("/p/a.rs");
        store.publish(1, path.clone(), vec![diag(Severity::Warning, 1, "w")]);
        store.publish(2, path.clone(), vec![diag(Severity::Error, 9, "e")]);
        let all = store.file(&path);
        let got: Vec<&str> = all.iter().map(|d| d.message.as_str()).collect();
        assert_eq!(got, vec!["e", "w"]);
        store.clear_server(2);
        assert_eq!(store.file(&path).len(), 1);
    }

    #[test]
    fn republish_replaces_the_server_entry() {
        let store = DiagnosticStore::default();
        let path = PathBuf::from("/p/a.rs");
        store.publish(1, path.clone(), vec![diag(Severity::Error, 1, "old")]);
        store.publish(1, path.clone(), Vec::new());
        assert_eq!(store.file(&path), Vec::new());
        assert_eq!(store.last_publish(&path), 2);
    }

    #[test]
    fn new_errors_ignore_the_edited_file_and_known_errors() {
        let store = DiagnosticStore::default();
        let edited = PathBuf::from("/p/a.rs");
        let other = PathBuf::from("/p/b.rs");
        store.publish(
            1,
            other.clone(),
            vec![diag(Severity::Error, 3, "already broken")],
        );
        let baseline = store.baseline_except(&edited);
        store.publish(1, edited.clone(), vec![diag(Severity::Error, 1, "mine")]);
        store.publish(
            1,
            other.clone(),
            vec![
                diag(Severity::Error, 3, "already broken"),
                diag(Severity::Error, 8, "broken by the edit"),
                diag(Severity::Warning, 9, "just a warning"),
            ],
        );
        let fresh = store.new_errors_since(&baseline, &edited);
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].path, other);
        assert_eq!(fresh[0].diagnostics.len(), 1);
        assert_eq!(fresh[0].diagnostics[0].message, "broken by the edit");
    }

    #[tokio::test(start_paused = true)]
    async fn waits_for_first_publish_then_settles() {
        let store = DiagnosticStore::default();
        let path = PathBuf::from("/p/a.rs");
        let baseline = store.seq();
        let publisher = store.clone();
        let publish_path = path.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            publisher.publish(
                1,
                publish_path.clone(),
                vec![diag(Severity::Error, 1, "first")],
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
            publisher.publish(1, publish_path, vec![diag(Severity::Error, 1, "second")]);
        });
        let started = Instant::now();
        let fresh = store
            .wait_for_publish(
                &path,
                baseline,
                Duration::from_millis(1000),
                Duration::from_millis(300),
                Duration::from_secs(3),
            )
            .await;
        assert!(fresh);
        // 200 ms first publish + 100 ms second + 300 ms of quiet.
        assert_eq!(started.elapsed(), Duration::from_millis(600));
        assert_eq!(store.file(&path)[0].message, "second");
    }

    #[tokio::test(start_paused = true)]
    async fn gives_up_after_the_first_window() {
        let store = DiagnosticStore::default();
        let started = Instant::now();
        let fresh = store
            .wait_for_publish(
                Path::new("/p/a.rs"),
                0,
                Duration::from_millis(1000),
                Duration::from_millis(300),
                Duration::from_secs(3),
            )
            .await;
        assert!(!fresh);
        assert_eq!(started.elapsed(), Duration::from_millis(1000));
    }

    #[tokio::test(start_paused = true)]
    async fn a_chatty_server_is_cut_off_by_the_cap() {
        let store = DiagnosticStore::default();
        let path = PathBuf::from("/p/a.rs");
        let publisher = store.clone();
        let publish_path = path.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                publisher.publish(1, publish_path.clone(), Vec::new());
            }
        });
        let started = Instant::now();
        let fresh = store
            .wait_for_publish(
                &path,
                0,
                Duration::from_millis(1000),
                Duration::from_millis(300),
                Duration::from_secs(2),
            )
            .await;
        assert!(fresh);
        assert!(started.elapsed() <= Duration::from_millis(2200));
    }
}
