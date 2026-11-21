//! In-process answers to the questions Nori asks of a git repository.
//!
//! Session start, an agent switch and the footer all want the same handful of facts: where the
//! working tree is, which branch and commit are checked out, what the remote is. Each used to be
//! a `git` process, tens of milliseconds apiece and a dependency on the binary being installed.
//! They are read here with `gix` instead.

use std::path::Path;
use std::path::PathBuf;

/// An open repository.
pub struct GitRepo {
    pub(crate) repo: gix::Repository,
}

impl GitRepo {
    /// The repository `path` is in, found by searching upwards like `git` does.
    ///
    /// Only the repository's own configuration is read, never the system or user files.
    pub fn discover(path: &Path) -> Option<Self> {
        let start = if path.is_dir() { path } else { path.parent()? };
        let repo =
            gix::discover_opts(start, Default::default(), gix::open::Options::isolated()).ok()?;
        Some(Self { repo })
    }

    /// The working tree's root (`git rev-parse --show-toplevel`); `None` for a bare repository.
    pub fn work_dir(&self) -> Option<PathBuf> {
        self.repo.workdir().map(Path::to_path_buf)
    }

    /// This working tree's git directory (`--git-dir`).
    pub fn git_dir(&self) -> &Path {
        self.repo.git_dir()
    }

    /// The git directory shared by every worktree (`--git-common-dir`).
    pub fn common_dir(&self) -> &Path {
        self.repo.common_dir()
    }

    /// Whether this is a linked worktree rather than the main working tree.
    pub fn is_linked_worktree(&self) -> bool {
        self.repo.kind() == gix::repository::Kind::LinkedWorkTree
    }

    /// The checked-out branch's short name; `None` when HEAD is detached or has no commit yet.
    pub fn branch(&self) -> Option<String> {
        self.head_commit()?;
        let name = self.repo.head_name().ok().flatten()?;
        (name.category() == Some(gix::reference::Category::LocalBranch))
            .then(|| name.shorten().to_string())
    }

    /// The commit HEAD points at, as full hex; `None` before the first commit.
    pub fn head_commit(&self) -> Option<String> {
        self.repo.head_id().ok().map(|id| id.to_string())
    }

    /// The URL of `remote`, with `url.<base>.insteadOf` rewrites applied like
    /// `git remote get-url`.
    ///
    /// The repository is reopened with the user's configuration for this one question, because
    /// those rewrites live there and the project id hashes the result. A different answer would
    /// quietly give an existing project a new identity.
    pub fn remote_url(&self, remote: &str) -> Option<String> {
        let work = self.repo.workdir().unwrap_or_else(|| self.repo.git_dir());
        let repo = gix::discover(work).ok()?;
        let remote = repo.find_remote(remote).ok()?;
        let url = remote.url(gix::remote::Direction::Fetch)?;
        let text = url.to_bstring().to_string();
        (!text.is_empty()).then_some(text)
    }

    /// The branch the remote's HEAD points at (`refs/remotes/<remote>/HEAD`), e.g. `main`.
    pub fn remote_default_branch(&self, remote: &str) -> Option<String> {
        let reference = self
            .repo
            .find_reference(&format!("refs/remotes/{remote}/HEAD"))
            .ok()?;
        let target = reference.target();
        let name = target.try_name()?;
        let prefix = format!("refs/remotes/{remote}/");
        name.as_bstr()
            .to_string()
            .strip_prefix(&prefix)
            .map(str::to_string)
    }

    /// Whether `name` is a local branch.
    pub fn has_local_branch(&self, name: &str) -> bool {
        self.repo
            .find_reference(&format!("refs/heads/{name}"))
            .is_ok()
    }

    /// Whether the working tree has files git does not track and does not ignore
    /// (`git ls-files --others --exclude-standard` printing anything).
    pub fn has_untracked_files(&self) -> bool {
        let Ok(platform) = self.repo.status(gix::progress::Discard) else {
            return false;
        };
        let Ok(items) = platform
            .untracked_files(gix::status::UntrackedFiles::Collapsed)
            .into_index_worktree_iter(Vec::<gix::bstr::BString>::new())
        else {
            return false;
        };
        items.flatten().any(|item| {
            matches!(
                item,
                gix::status::index_worktree::Item::DirectoryContents { entry, .. }
                    if entry.status == gix::dir::entry::Status::Untracked
            )
        })
    }

    /// Lines added and removed in tracked files since HEAD, staged or not
    /// (`git diff HEAD --shortstat`). `None` before the first commit or outside a working tree.
    ///
    /// A file moved without edits counts as nothing, as it does for git's rename detection.
    pub fn diff_head_stats(&self) -> Option<(u32, u32)> {
        let changes = self.head_changes()?;
        let (mut added, mut removed) = (0u32, 0u32);
        let removed_only: Vec<&[u8]> = changes
            .iter()
            .filter_map(|c| c.new.is_none().then_some(c.old.as_deref()).flatten())
            .collect();
        let added_only: Vec<&[u8]> = changes
            .iter()
            .filter_map(|c| c.old.is_none().then_some(c.new.as_deref()).flatten())
            .collect();
        for change in &changes {
            let (old, new) = (change.old.as_deref(), change.new.as_deref());
            // An unedited move shows as one deletion and one addition of the same content.
            let moved = match (old, new) {
                (Some(old), None) => added_only.contains(&old),
                (None, Some(new)) => removed_only.contains(&new),
                _ => false,
            };
            if moved {
                continue;
            }
            let (a, r) = count_lines(old.unwrap_or_default(), new.unwrap_or_default());
            added = added.saturating_add(a);
            removed = removed.saturating_add(r);
        }
        Some((added, removed))
    }

    /// The revision a pull-request-style diff starts from: the merge-base of HEAD with the
    /// remote's default branch, else with `main`, else with `master`, else HEAD itself.
    pub fn pr_diff_base(&self) -> String {
        if let Some(default) = self.remote_default_branch("origin")
            && let Some(base) = self.merge_base("HEAD", &format!("origin/{default}"))
        {
            return base;
        }
        for branch in ["main", "master"] {
            if let Some(base) = self.merge_base("HEAD", branch) {
                return base;
            }
        }
        "HEAD".to_string()
    }

    /// A unified diff of the working tree against the revision `base`, in the layout of
    /// `git diff <base>` followed by every untracked file as a new file. With `color`, it uses
    /// git's default palette. `None` when `base` does not name a commit.
    pub fn diff_text(&self, base: &str, color: bool) -> Option<String> {
        let palette = Palette::new(color);
        let mut out = String::new();
        for file in self.versions_since(base)? {
            write_file_diff(
                &mut out,
                &palette,
                &file.path,
                file.old.as_ref().map(|(id, data)| (*id, data.as_slice())),
                file.old_mode,
                file.new.as_deref(),
                file.new_mode,
            );
        }
        Some(out)
    }

    /// Every file of the working tree that differs from the revision `base` (untracked files
    /// included, last), with both versions: what a diff of them shows. `None` when `base` does
    /// not name a commit.
    pub fn file_versions_since(&self, base: &str) -> Option<Vec<FileVersions>> {
        Some(
            self.versions_since(base)?
                .into_iter()
                .map(|file| FileVersions {
                    path: file.path,
                    old: file.old.map(|(_, data)| data),
                    new: file.new,
                })
                .collect(),
        )
    }

    fn versions_since(&self, base: &str) -> Option<Vec<Versions>> {
        let workdir = self.repo.workdir()?.to_path_buf();
        let base_tree = match self
            .repo
            .rev_parse_single(base)
            .ok()
            .and_then(|id| id.object().ok()?.peel_to_commit().ok()?.tree().ok())
        {
            Some(tree) => tree,
            // Before the first commit everything in the working tree is new.
            None if self.head_commit().is_none() => self.repo.empty_tree(),
            None => return None,
        };
        let head_tree = self.repo.head_tree().ok();

        // Every path whose content may differ from `base`: what moved between `base` and HEAD,
        // plus what changed since HEAD.
        let mut paths: std::collections::BTreeSet<gix::bstr::BString> = Default::default();
        if let Some(head_tree) = &head_tree {
            for change in self
                .repo
                .diff_tree_to_tree(&base_tree, head_tree, None)
                .ok()?
            {
                use gix::object::tree::diff::ChangeDetached as Change;
                match change {
                    Change::Addition { location, .. }
                    | Change::Deletion { location, .. }
                    | Change::Modification { location, .. } => {
                        paths.insert(location);
                    }
                    Change::Rewrite {
                        location,
                        source_location,
                        ..
                    } => {
                        paths.insert(location);
                        paths.insert(source_location);
                    }
                }
            }
        }
        let mut untracked = Vec::new();
        let items = self
            .repo
            .status(gix::progress::Discard)
            .ok()?
            .untracked_files(gix::status::UntrackedFiles::Files)
            .into_iter(Vec::<gix::bstr::BString>::new())
            .ok()?;
        for item in items.flatten() {
            match item {
                gix::status::Item::TreeIndex(change) => {
                    paths.insert(change.location().to_owned());
                }
                gix::status::Item::IndexWorktree(
                    gix::status::index_worktree::Item::Modification { rela_path, .. },
                ) => {
                    paths.insert(rela_path);
                }
                gix::status::Item::IndexWorktree(
                    gix::status::index_worktree::Item::DirectoryContents { entry, .. },
                ) if entry.status == gix::dir::entry::Status::Untracked
                    && entry.disk_kind == Some(gix::dir::entry::Kind::File) =>
                {
                    untracked.push(entry.rela_path.clone());
                }
                _ => {}
            }
        }
        untracked.sort();

        let mut versions = Vec::new();
        for path in paths {
            let Ok(relative) = gix::path::from_bstr(path.as_ref() as &gix::bstr::BStr) else {
                continue;
            };
            let entry = base_tree
                .lookup_entry_by_path(&relative)
                .ok()
                .flatten()
                .filter(|entry| entry.mode().is_blob());
            let old = entry
                .as_ref()
                .and_then(|entry| entry.object().ok())
                .map(|object| (object.id, object.data.clone()));
            let old_mode = entry.as_ref().map(|entry| u32::from(entry.mode().value()));
            let full = workdir.join(&relative);
            let new = std::fs::read(&full).ok();
            let new_mode = new.as_ref().map(|_| worktree_mode(&full));
            versions.push(Versions {
                path: path.to_string(),
                old,
                old_mode,
                new,
                new_mode,
            });
        }
        for path in untracked {
            let Ok(relative) = gix::path::from_bstr(path.as_ref() as &gix::bstr::BStr) else {
                continue;
            };
            let full = workdir.join(&relative);
            let Ok(content) = std::fs::read(&full) else {
                continue;
            };
            versions.push(Versions {
                path: path.to_string(),
                old: None,
                old_mode: None,
                new: Some(content),
                new_mode: Some(worktree_mode(&full)),
            });
        }
        Some(versions)
    }

    /// The tracked files that differ between HEAD and the working tree, with both versions.
    fn head_changes(&self) -> Option<Vec<FileChange>> {
        let head_tree = self.repo.head_tree().ok()?;
        let workdir = self.repo.workdir()?.to_path_buf();
        let items = self
            .repo
            .status(gix::progress::Discard)
            .ok()?
            .untracked_files(gix::status::UntrackedFiles::None)
            .into_iter(Vec::<gix::bstr::BString>::new())
            .ok()?;

        let mut paths = std::collections::BTreeSet::new();
        for item in items.flatten() {
            match item {
                gix::status::Item::TreeIndex(change) => {
                    paths.insert(change.location().to_owned());
                }
                gix::status::Item::IndexWorktree(
                    gix::status::index_worktree::Item::Modification { rela_path, .. },
                ) => {
                    paths.insert(rela_path);
                }
                _ => {}
            }
        }

        let mut changes = Vec::new();
        for path in paths {
            let Ok(relative) = gix::path::from_bstr(path.as_ref() as &gix::bstr::BStr) else {
                continue;
            };
            let old = head_tree
                .lookup_entry_by_path(&relative)
                .ok()
                .flatten()
                .filter(|entry| entry.mode().is_blob())
                .and_then(|entry| entry.object().ok())
                .map(|object| object.data.clone());
            let new = std::fs::read(workdir.join(&relative)).ok();
            if old != new {
                changes.push(FileChange { old, new });
            }
        }
        Some(changes)
    }

    /// The best common ancestor of two revisions (`git merge-base a b`), as full hex.
    pub fn merge_base(&self, a: &str, b: &str) -> Option<String> {
        let a = self.repo.rev_parse_single(a).ok()?.detach();
        let b = self.repo.rev_parse_single(b).ok()?.detach();
        self.repo
            .merge_base(a, b)
            .ok()
            .flatten()
            .map(|id| id.to_string())
    }
}

/// A file of the working tree that differs from a revision: its two versions, either of which
/// may be missing (added, deleted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileVersions {
    /// Relative to the work tree.
    pub path: String,
    pub old: Option<Vec<u8>>,
    pub new: Option<Vec<u8>>,
}

/// [`FileVersions`] with what a textual diff header needs too.
struct Versions {
    path: String,
    old: Option<(gix::ObjectId, Vec<u8>)>,
    old_mode: Option<u32>,
    new: Option<Vec<u8>>,
    new_mode: Option<u32>,
}

/// ANSI sequences for the parts of a diff, or nothing at all.
struct Palette {
    meta: &'static str,
    frag: &'static str,
    old: &'static str,
    new: &'static str,
    reset: &'static str,
}

impl Palette {
    fn new(color: bool) -> Self {
        if color {
            Self {
                meta: "\x1b[1m",
                frag: "\x1b[36m",
                old: "\x1b[31m",
                new: "\x1b[32m",
                reset: "\x1b[m",
            }
        } else {
            Self {
                meta: "",
                frag: "",
                old: "",
                new: "",
                reset: "",
            }
        }
    }
}

/// The git file mode of a working tree file: executable or plain.
fn worktree_mode(path: &Path) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if std::fs::metadata(path).is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0) {
            return 0o100755;
        }
    }
    let _ = path;
    0o100644
}

/// Append one file's diff, in the layout of `git diff`.
fn write_file_diff(
    out: &mut String,
    palette: &Palette,
    path: &str,
    old: Option<(gix::ObjectId, &[u8])>,
    old_mode: Option<u32>,
    new: Option<&[u8]>,
    new_mode: Option<u32>,
) {
    use std::fmt::Write as _;

    let Palette { meta, reset, .. } = palette;
    let old_bytes = old.map(|(_, data)| data);
    if old_bytes == new {
        return;
    }
    // Hashing a blob cannot fail; if it somehow does, the index line shows no id.
    let new_id = new.and_then(|data| {
        gix::objs::compute_hash(gix::hash::Kind::Sha1, gix::object::Kind::Blob, data).ok()
    });
    let abbrev = |id: Option<gix::ObjectId>| {
        id.map_or_else(
            || "0000000".to_string(),
            |id| id.to_hex_with_len(7).to_string(),
        )
    };
    let _ = writeln!(out, "{meta}diff --git a/{path} b/{path}{reset}");
    match (old_mode, new_mode) {
        (None, Some(mode)) => {
            let _ = writeln!(out, "{meta}new file mode {mode:o}{reset}");
        }
        (Some(mode), None) => {
            let _ = writeln!(out, "{meta}deleted file mode {mode:o}{reset}");
        }
        (Some(before), Some(after)) if before != after => {
            let _ = writeln!(out, "{meta}old mode {before:o}{reset}");
            let _ = writeln!(out, "{meta}new mode {after:o}{reset}");
        }
        _ => {}
    }
    let mode_suffix = match (old_mode, new_mode) {
        (Some(before), Some(after)) if before == after => format!(" {after:o}"),
        _ => String::new(),
    };
    let _ = writeln!(
        out,
        "{meta}index {}..{}{mode_suffix}{reset}",
        abbrev(old.map(|(id, _)| id)),
        abbrev(new_id)
    );

    let (old_data, new_data) = (old_bytes.unwrap_or_default(), new.unwrap_or_default());
    if old_data.contains(&0) || new_data.contains(&0) {
        let a = if old.is_some() {
            format!("a/{path}")
        } else {
            "/dev/null".into()
        };
        let b = if new.is_some() {
            format!("b/{path}")
        } else {
            "/dev/null".into()
        };
        let _ = writeln!(out, "Binary files {a} and {b} differ");
        return;
    }
    let a = if old.is_some() {
        format!("a/{path}")
    } else {
        "/dev/null".into()
    };
    let b = if new.is_some() {
        format!("b/{path}")
    } else {
        "/dev/null".into()
    };
    let _ = writeln!(out, "{meta}--- {a}{reset}");
    let _ = writeln!(out, "{meta}+++ {b}{reset}");

    let input = imara_diff::InternedInput::new(
        imara_diff::sources::byte_lines(old_data),
        imara_diff::sources::byte_lines(new_data),
    );
    let diff = imara_diff::Diff::compute(imara_diff::Algorithm::Histogram, &input);
    let printer = LinePrinter {
        interner: &input.interner,
        before: &input.before,
        palette,
    };
    let _ = write!(
        out,
        "{}",
        diff.unified_diff(&printer, imara_diff::UnifiedDiffConfig::default(), &input)
    );
}

/// Prints diff lines the way `git diff` does, optionally in color.
struct LinePrinter<'a> {
    interner: &'a imara_diff::Interner<&'a [u8]>,
    /// The old file's lines, to find the function a hunk sits in.
    before: &'a [imara_diff::Token],
    palette: &'a Palette,
}

impl LinePrinter<'_> {
    /// One diff line. `color` is the line's palette entry, empty for context. git colors an
    /// added line's `+` separately from its text, and ends every colored line with a reset.
    fn line(
        &self,
        f: &mut impl std::fmt::Write,
        prefix: char,
        color: &str,
        token: imara_diff::Token,
    ) -> std::fmt::Result {
        let bytes = self.interner[token];
        let (text, newline) = match bytes.strip_suffix(b"\n") {
            Some(text) => (text, true),
            None => (bytes, false),
        };
        let text = String::from_utf8_lossy(text);
        let reset = self.palette.reset;
        if prefix == '+' && !reset.is_empty() {
            write!(f, "{color}+{reset}")?;
            if !text.is_empty() {
                write!(f, "{color}{text}{reset}")?;
            }
            writeln!(f)?;
        } else {
            writeln!(f, "{color}{prefix}{text}{reset}")?;
        }
        if !newline {
            writeln!(f, "\\ No newline at end of file{reset}")?;
        }
        Ok(())
    }
}

impl imara_diff::UnifiedDiffPrinter for LinePrinter<'_> {
    fn display_header(
        &self,
        mut f: impl std::fmt::Write,
        start_before: u32,
        start_after: u32,
        len_before: u32,
        len_after: u32,
    ) -> std::fmt::Result {
        // `git` writes a length of one as just the start.
        let span = |start: u32, len: u32| match len {
            1 => format!("{}", start + 1),
            0 => format!("{start},0"),
            len => format!("{},{len}", start + 1),
        };
        // git's default hint: the closest earlier line that starts like a declaration.
        let hint = self.before[..start_before as usize]
            .iter()
            .rev()
            .map(|&token| self.interner[token])
            .find(|line| {
                line.first()
                    .is_some_and(|&b| b.is_ascii_alphabetic() || b == b'$' || b == b'_')
            })
            .map(|line| {
                let text = String::from_utf8_lossy(line);
                let text = text.trim_end();
                let text = text.chars().take(80).collect::<String>();
                format!(" {}{text}{}", self.palette.reset, self.palette.reset)
            })
            .unwrap_or_default();
        let Palette { frag, reset, .. } = self.palette;
        writeln!(
            f,
            "{frag}@@ -{} +{} @@{reset}{hint}",
            span(start_before, len_before),
            span(start_after, len_after)
        )
    }

    fn display_context_token(
        &self,
        mut f: impl std::fmt::Write,
        token: imara_diff::Token,
    ) -> std::fmt::Result {
        self.line(&mut f, ' ', "", token)
    }

    fn display_hunk(
        &self,
        mut f: impl std::fmt::Write,
        before: &[imara_diff::Token],
        after: &[imara_diff::Token],
    ) -> std::fmt::Result {
        for &token in before {
            self.line(&mut f, '-', self.palette.old, token)?;
        }
        for &token in after {
            self.line(&mut f, '+', self.palette.new, token)?;
        }
        Ok(())
    }
}

/// One tracked file's content at HEAD and in the working tree; `None` when it does not exist there.
struct FileChange {
    old: Option<Vec<u8>>,
    new: Option<Vec<u8>>,
}

/// Lines added and removed going from `old` to `new`. Binary content counts as no lines, as in
/// `git diff --shortstat`.
pub(crate) fn count_lines(old: &[u8], new: &[u8]) -> (u32, u32) {
    if old.contains(&0) || new.contains(&0) {
        return (0, 0);
    }
    let input = imara_diff::InternedInput::new(old, new);
    let diff = imara_diff::Diff::compute(imara_diff::Algorithm::Histogram, &input);
    (diff.count_additions(), diff.count_removals())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// A repository with one commit on `main`, built without the `git` binary.
    fn repo_with_commit(dir: &Path) -> gix::Repository {
        let mut repo = gix::init(dir).expect("init");
        {
            let mut config = repo.config_snapshot_mut();
            config.set_raw_value("user.name", "Test").expect("name");
            config
                .set_raw_value("user.email", "test@example.com")
                .expect("email");
        }
        let tree = repo.empty_tree().id;
        repo.commit("refs/heads/main", "first", tree, None::<gix::ObjectId>)
            .expect("commit");
        std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/main\n").expect("head");
        repo
    }

    /// Commit `files` on `main` and check them out into the index, without the `git` binary.
    fn commit_files(repo: &gix::Repository, files: &[(&str, &str)]) {
        let dir = repo.workdir().expect("workdir").to_path_buf();
        let mut editor = repo.edit_tree(repo.empty_tree().id).expect("editor");
        for (path, content) in files {
            let blob = repo.write_blob(content.as_bytes()).expect("blob").detach();
            editor
                .upsert(*path, gix::object::tree::EntryKind::Blob, blob)
                .expect("upsert");
            let full = dir.join(path);
            std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
            std::fs::write(full, content).expect("write");
        }
        let tree = editor.write().expect("tree");
        let parent = repo.head_id().ok().map(gix::Id::detach);
        repo.commit("refs/heads/main", "files", tree, parent)
            .expect("commit");
        let mut index = repo.index_from_tree(&tree).expect("index");
        index.write(Default::default()).expect("write index");
    }

    #[test]
    fn reads_the_facts_of_a_repository() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().canonicalize().expect("canonical");
        let repo = repo_with_commit(&dir);
        let head = repo.head_id().expect("head").to_string();

        let git = GitRepo::discover(&dir).expect("discovered");
        assert_eq!(git.work_dir(), Some(dir));
        assert_eq!(git.branch().as_deref(), Some("main"));
        assert_eq!(git.head_commit(), Some(head));
        assert!(!git.is_linked_worktree());
        assert!(git.has_local_branch("main"));
        assert!(!git.has_local_branch("other"));
        assert_eq!(git.remote_url("origin"), None);
    }

    #[test]
    fn a_repository_with_no_commit_has_no_branch_or_head() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().canonicalize().expect("canonical");
        gix::init(&dir).expect("init");

        let git = GitRepo::discover(&dir).expect("discovered");
        assert_eq!(git.branch(), None);
        assert_eq!(git.head_commit(), None);
    }

    #[test]
    fn a_directory_outside_any_repository_is_not_one() {
        let temp = tempfile::tempdir().expect("tempdir");
        assert!(GitRepo::discover(temp.path()).is_none());
    }

    #[test]
    fn a_subdirectory_finds_its_repository() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().canonicalize().expect("canonical");
        repo_with_commit(&dir);
        let nested = dir.join("a/b");
        std::fs::create_dir_all(&nested).expect("mkdir");

        let git = GitRepo::discover(&nested).expect("discovered");
        assert_eq!(git.work_dir(), Some(dir));
    }

    #[test]
    fn the_remote_url_and_its_default_branch_are_read() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().canonicalize().expect("canonical");
        let repo = repo_with_commit(&dir);
        let head = repo.head_id().expect("head");
        std::fs::write(
            dir.join(".git/config"),
            std::fs::read_to_string(dir.join(".git/config")).expect("config")
                + "[remote \"origin\"]\n\turl = git@example.com:team/repo.git\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n",
        )
        .expect("write config");
        repo.reference(
            "refs/remotes/origin/main",
            head,
            gix::refs::transaction::PreviousValue::Any,
            "test",
        )
        .expect("remote branch");
        std::fs::create_dir_all(dir.join(".git/refs/remotes/origin")).expect("mkdir");
        std::fs::write(
            dir.join(".git/refs/remotes/origin/HEAD"),
            "ref: refs/remotes/origin/main\n",
        )
        .expect("origin head");

        let git = GitRepo::discover(&dir).expect("discovered");
        assert_eq!(
            git.remote_url("origin").as_deref(),
            Some("git@example.com:team/repo.git")
        );
        assert_eq!(git.remote_default_branch("origin").as_deref(), Some("main"));
        assert_eq!(git.remote_url("missing"), None);
    }

    #[test]
    fn finds_the_merge_base_of_two_commits() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().canonicalize().expect("canonical");
        let repo = repo_with_commit(&dir);
        let base = repo.head_id().expect("head");
        let tree = repo.empty_tree().id;
        let child = repo
            .commit("refs/heads/feature", "child", tree, [base.detach()])
            .expect("child");

        let git = GitRepo::discover(&dir).expect("discovered");
        assert_eq!(
            git.merge_base("main", "feature"),
            Some(base.to_string()),
            "child is {child}"
        );
    }

    #[test]
    fn untracked_files_are_noticed_and_tracked_ones_are_not() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().canonicalize().expect("canonical");
        let repo = repo_with_commit(&dir);
        commit_files(&repo, &[("tracked.txt", "one\n")]);

        let git = GitRepo::discover(&dir).expect("discovered");
        assert!(!git.has_untracked_files());

        std::fs::write(dir.join("new.txt"), "x").expect("write");
        assert!(git.has_untracked_files());
    }

    #[test]
    fn diff_stats_count_lines_changed_since_head() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().canonicalize().expect("canonical");
        let repo = repo_with_commit(&dir);
        commit_files(&repo, &[("a.txt", "1\n2\n3\n"), ("b.txt", "keep\n")]);

        let git = GitRepo::discover(&dir).expect("discovered");
        assert_eq!(git.diff_head_stats(), Some((0, 0)));

        // One line replaced and one added in a.txt; b.txt deleted; an untracked file ignored.
        std::fs::write(dir.join("a.txt"), "1\nTWO\n3\n4\n").expect("edit");
        std::fs::remove_file(dir.join("b.txt")).expect("delete");
        std::fs::write(dir.join("untracked.txt"), "ignored\nby the stat\n").expect("untracked");
        assert_eq!(git.diff_head_stats(), Some((2, 2)));
    }

    #[test]
    fn an_unstaged_move_shows_only_its_deletion_like_git() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().canonicalize().expect("canonical");
        let repo = repo_with_commit(&dir);
        commit_files(&repo, &[("old.txt", "a\nb\nc\n")]);

        std::fs::rename(dir.join("old.txt"), dir.join("moved.txt")).expect("move");
        let git = GitRepo::discover(&dir).expect("discovered");
        // The new name is untracked, so only the deletion counts, exactly as in
        // `git diff HEAD --shortstat` before the move is staged.
        let stats = git.diff_head_stats().expect("stats");
        assert_eq!(stats.1, 3);
    }

    #[test]
    fn there_are_no_stats_before_the_first_commit() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().canonicalize().expect("canonical");
        gix::init(&dir).expect("init");
        assert_eq!(
            GitRepo::discover(&dir).expect("repo").diff_head_stats(),
            None
        );
    }

    /// Our diff must be byte-for-byte what `git diff --color` prints, so the `git` binary is used
    /// here only as the reference to compare against.
    #[test]
    fn the_colored_diff_matches_git_byte_for_byte() {
        use std::process::Command;
        let git = |dir: &Path, args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .expect("git runs");
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout).expect("utf8")
        };
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().canonicalize().expect("canonical");
        git(&dir, &["init", "-q", "-b", "main"]);
        git(&dir, &["config", "user.email", "t@example.com"]);
        git(&dir, &["config", "user.name", "T"]);
        let numbered: String = (1..=40).map(|n| format!("line {n}\n")).collect();
        std::fs::write(dir.join("long.txt"), &numbered).expect("long");
        std::fs::write(dir.join("code.rs"), "fn alpha() {\n    one();\n    two();\n}\n\nfn beta() {\n    three();\n    four();\n}\n").expect("code");
        std::fs::write(dir.join("gone.txt"), "bye\n").expect("gone");
        std::fs::write(dir.join("tail.txt"), "no newline at end").expect("tail");
        std::fs::write(dir.join("bin.dat"), [0u8, 1, 2, 3]).expect("bin");
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-q", "-m", "base"]);

        let edited: String = numbered
            .replace("line 3\n", "line three\n")
            .replace("line 30\n", "line thirty\nextra\n");
        std::fs::write(dir.join("long.txt"), edited).expect("edit long");
        std::fs::write(dir.join("code.rs"), "fn alpha() {\n    one();\n    two();\n}\n\nfn beta() {\n    three();\n    FOUR();\n}\n").expect("edit code");
        std::fs::remove_file(dir.join("gone.txt")).expect("rm");
        std::fs::write(dir.join("tail.txt"), "no newline at the end").expect("edit tail");
        std::fs::write(dir.join("bin.dat"), [0u8, 9, 9, 9]).expect("edit bin");

        let git_repo = GitRepo::discover(&dir).expect("repo");
        let ours = git_repo.diff_text("HEAD", true).expect("diff");
        let theirs = git(&dir, &["diff", "--color", "HEAD"]);
        assert_eq!(ours, theirs);
    }
}
