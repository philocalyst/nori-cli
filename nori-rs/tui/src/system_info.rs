use std::env;
use std::process::Command;
use std::sync::OnceLock;

/// Indicates which command was used to detect the Nori version.
/// This affects the UI display text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum NoriVersionSource {
    /// Version from `nori-skillsets` command (new installer) - displays as "Skillsets"
    #[default]
    Skillsets,
    /// Version from the legacy `nori-ai` skillset installer.
    LegacySkillsets,
}

impl NoriVersionSource {
    /// Returns the display label for this version source.
    pub(crate) fn label(self) -> &'static str {
        match self {
            NoriVersionSource::Skillsets => "Skillsets",
            NoriVersionSource::LegacySkillsets => "Legacy Skillsets",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct SystemInfo {
    pub(crate) git_branch: Option<String>,
    pub(crate) active_skillsets: Vec<String>,
    pub(crate) nori_version: Option<String>,
    /// Indicates which command was used to detect the version (affects UI display).
    pub(crate) nori_version_source: Option<NoriVersionSource>,
    pub(crate) git_lines_added: Option<i32>,
    pub(crate) git_lines_removed: Option<i32>,
    /// Whether there are untracked, non-ignored files in the current git repo.
    pub(crate) git_has_untracked: bool,
    /// Whether the current directory is a git worktree (not the main repo)
    pub(crate) is_worktree: bool,
    /// The worktree directory name (last path component when parent is `.worktrees/`)
    pub(crate) worktree_name: Option<String>,
    /// Warning about low disk space with worktrees present (session-start check)
    pub(crate) worktree_cleanup_warning: Option<WorktreeCleanupWarning>,
}

impl SystemInfo {
    /// Collect system info synchronously (blocking).
    /// Only available in debug builds for E2E testing via NORI_SYNC_SYSTEM_INFO=1.
    #[cfg(debug_assertions)]
    pub fn collect_sync() -> Self {
        Self::collect_fresh()
    }

    /// The subset of system info worth blocking startup for: the git identity
    /// of `dir`, read in-process.
    ///
    /// Everything else a full collection does is too slow to run before the
    /// first frame — the skillset version spawns Node and the worktree disk
    /// check walks the filesystem — so those fields stay at
    /// their defaults until the background refresh overwrites this snapshot
    /// wholesale.
    pub(crate) fn seed(dir: &std::path::Path) -> Self {
        // E2E snapshots pin the complete, deterministic snapshot, so debug
        // builds under `NORI_SYNC_SYSTEM_INFO=1` still collect everything.
        #[cfg(debug_assertions)]
        if env::var("NORI_SYNC_SYSTEM_INFO").is_ok() {
            return Self::collect_sync();
        }

        let is_worktree = is_git_worktree(Some(dir));
        Self {
            git_branch: get_git_branch(Some(dir)),
            is_worktree,
            worktree_name: if is_worktree {
                extract_worktree_name(dir)
            } else {
                None
            },
            ..Self::default()
        }
    }

    /// Collect fresh system info for the current directory. Blocking: call it from a
    /// background thread.
    pub(crate) fn collect_fresh() -> Self {
        Self::collect_impl(None)
    }

    /// Collect the footer's facts about `dir`: its git state, skillsets and the Nori version.
    pub(crate) fn collect_for_directory(dir: &std::path::Path) -> Self {
        Self::collect_impl(Some(dir))
    }

    fn collect_impl(dir: Option<&std::path::Path>) -> Self {
        let (git_lines_added, git_lines_removed) = get_git_stats(dir);
        let (nori_version, nori_version_source) = get_nori_version();

        #[cfg(unix)]
        let worktree_cleanup_warning = {
            let effective_dir = dir
                .map(std::path::PathBuf::from)
                .or_else(|| env::current_dir().ok());
            effective_dir.and_then(|d| check_worktree_cleanup(&d))
        };
        #[cfg(not(unix))]
        let worktree_cleanup_warning = None;

        let is_worktree = is_git_worktree(dir);

        Self {
            git_branch: get_git_branch(dir),
            active_skillsets: get_active_skillsets(dir),
            nori_version,
            nori_version_source,
            git_lines_added,
            git_lines_removed,
            git_has_untracked: has_untracked_files(dir),
            is_worktree,
            worktree_name: if is_worktree {
                dir.and_then(extract_worktree_name)
            } else {
                None
            },
            worktree_cleanup_warning,
        }
    }
}

/// Cached result of `nori-skillsets --version` / `nori-ai --version`.
///
/// The installed CLI version is stable for the lifetime of a TUI process, so
/// we detect it once and reuse the result on every subsequent `SystemInfo`
/// refresh. This avoids spawning a heavyweight Node.js subprocess on every
/// footer update.
static NORI_VERSION_CACHE: OnceLock<(Option<String>, Option<NoriVersionSource>)> = OnceLock::new();

fn get_nori_version() -> (Option<String>, Option<NoriVersionSource>) {
    NORI_VERSION_CACHE.get_or_init(detect_nori_version).clone()
}

fn detect_nori_version() -> (Option<String>, Option<NoriVersionSource>) {
    // Try nori-skillsets first (new installer)
    if let Ok(output) = Command::new("nori-skillsets").arg("--version").output()
        && output.status.success()
        && let Some(version) = String::from_utf8(output.stdout)
            .ok()
            .and_then(|s| parse_nori_version(&s))
    {
        return (Some(version), Some(NoriVersionSource::Skillsets));
    }

    // Fallback to nori-ai (legacy installer)
    if let Ok(output) = Command::new("nori-ai").arg("--version").output()
        && output.status.success()
        && let Some(version) = String::from_utf8(output.stdout)
            .ok()
            .and_then(|s| parse_nori_version(&s))
    {
        return (Some(version), Some(NoriVersionSource::LegacySkillsets));
    }

    (None, None)
}

fn parse_nori_version(output: &str) -> Option<String> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return None;
    }

    let tokens: Vec<&str> = trimmed.split_whitespace().collect();

    match tokens.len() {
        // Format: "19.1.1" (version only)
        1 => Some(tokens[0].to_string()),
        // Format: "nori-ai 19.1.1" (program name + version)
        2 => Some(tokens[1].to_string()),
        // Unexpected format
        _ => None,
    }
}

/// Parse the stdout of `nori-skillsets list-active` into a list of skillset names.
///
/// Expects one skillset name per line. Trims whitespace and skips blank lines.
fn parse_active_skillsets(output: &str) -> Vec<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// Get active skillsets by running `nori-skillsets list-active`.
///
/// Falls back to reading `.nori-config.json` if `list-active` is not supported
/// (older versions of nori-skillsets). Returns an empty vec if no skillsets are
/// active or nori-skillsets is not installed.
fn get_active_skillsets(dir: Option<&std::path::Path>) -> Vec<String> {
    let mut cmd = Command::new("nori-skillsets");
    cmd.arg("list-active");
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    match cmd.output() {
        Ok(output) if output.status.success() => String::from_utf8(output.stdout)
            .ok()
            .map(|s| parse_active_skillsets(&s))
            .unwrap_or_default(),
        Ok(output) => {
            // Non-zero exit. Distinguish "no active skillsets" (known command,
            // no results) from "unknown subcommand" (old nori-skillsets version).
            // When the subcommand is unknown, the CLI framework writes to stderr.
            let has_stderr = output.stderr.iter().any(|&b| !b.is_ascii_whitespace());
            if has_stderr {
                // Likely an old version that doesn't support list-active.
                // Fall back to reading .nori-config.json directly.
                dir.map(std::path::Path::to_path_buf)
                    .or_else(|| env::current_dir().ok())
                    .and_then(|cwd| read_active_skillset(&cwd))
                    .into_iter()
                    .collect()
            } else {
                // Known command, just no active skillsets.
                Vec::new()
            }
        }
        // nori-skillsets not installed at all.
        Err(_) => Vec::new(),
    }
}

/// Read the active skillset from `.nori-config.json` by walking parent directories.
///
/// This is the legacy fallback for older versions of nori-skillsets that don't
/// support the `list-active` subcommand.
pub(crate) fn read_active_skillset(cwd: &std::path::Path) -> Option<String> {
    let mut current_dir = cwd.to_path_buf();

    loop {
        let config_path = current_dir.join(".nori-config.json");
        if config_path.exists()
            && let Ok(contents) = std::fs::read_to_string(&config_path)
            && let Ok(json) = serde_json::from_str::<serde_json::Value>(&contents)
            && let Some(skillset) = json.get("activeSkillset").and_then(|value| value.as_str())
        {
            return Some(skillset.to_string());
        }

        if !current_dir.pop() {
            break;
        }
    }

    None
}

/// Detect untracked files (files not yet added to git and not ignored).
fn has_untracked_files(dir: Option<&std::path::Path>) -> bool {
    git_repo(dir).is_some_and(|repo| repo.has_untracked_files())
}

/// Lines added and removed since HEAD, as `(added, removed)`; `None` for a side with no change.
fn get_git_stats(dir: Option<&std::path::Path>) -> (Option<i32>, Option<i32>) {
    let Some((added, removed)) = git_repo(dir).and_then(|repo| repo.diff_head_stats()) else {
        return (None, None);
    };
    let count = |lines: u32| (lines > 0).then(|| i32::try_from(lines).unwrap_or(i32::MAX));
    (count(added), count(removed))
}

/// The checked-out branch, as `git branch --show-current` reports it: `None` when HEAD is
/// detached or there is no repository. Long names are shortened to 30 characters. Read with `gix`,
/// without starting `git`, because the footer asks before the first frame.
fn get_git_branch(dir: Option<&std::path::Path>) -> Option<String> {
    let facts = git_facts(dir)?;
    let branch = facts.current_branch()?;
    if branch.chars().count() > 30 {
        Some(format!(
            "{}...",
            branch.chars().take(27).collect::<String>()
        ))
    } else {
        Some(branch.to_string())
    }
}

/// Disk space threshold: warn when free space is below this percentage.
const DISK_SPACE_LOW_PERCENT: i32 = 10;

/// Disk space information parsed from `df` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiskSpaceInfo {
    pub(crate) used_percent: i32,
}

/// Warning about low disk space when git worktrees exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorktreeCleanupWarning {
    pub(crate) worktree_count: usize,
    pub(crate) free_percent: i32,
}

/// How full the filesystem holding `dir` is: used space over used plus the space available to
/// unprivileged users, rounded up, as POSIX `df` computes it. On APFS this is the whole
/// container's figure, where `df` shows only the volume's own share.
#[cfg(unix)]
fn get_disk_space(dir: &std::path::Path) -> Option<DiskSpaceInfo> {
    use std::os::unix::ffi::OsStrExt;

    let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is a valid NUL-terminated string and `stats` is a valid out-pointer.
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: statvfs returned success, so the struct is initialized.
    let stats = unsafe { stats.assume_init() };
    let used_percent = used_percent(
        u64::from(stats.f_blocks),
        u64::from(stats.f_bfree),
        u64::from(stats.f_bavail),
    )?;
    Some(DiskSpaceInfo { used_percent })
}

/// POSIX `df` capacity: `ceil(used * 100 / (used + available))`.
fn used_percent(blocks: u64, free: u64, available: u64) -> Option<i32> {
    let used = blocks.checked_sub(free)?;
    let denominator = used.checked_add(available)?;
    if denominator == 0 {
        return None;
    }
    i32::try_from((used * 100).div_ceil(denominator)).ok()
}

/// Evaluate whether a worktree cleanup warning should be shown.
///
/// Takes the number of extra worktrees and the disk usage percentage.
/// Returns `Some(WorktreeCleanupWarning)` if worktrees > 0 and free space < threshold.
fn evaluate_worktree_cleanup_warning(
    worktree_count: usize,
    used_percent: i32,
) -> Option<WorktreeCleanupWarning> {
    if worktree_count == 0 {
        return None;
    }

    let free_percent = (100 - used_percent).max(0);
    if free_percent >= DISK_SPACE_LOW_PERCENT {
        return None;
    }

    Some(WorktreeCleanupWarning {
        worktree_count,
        free_percent,
    })
}

/// Check if disk space is low and worktrees exist for the given directory.
///
/// This is called during background system info collection. Returns a warning
/// if worktrees are present and disk space is below the threshold.
#[cfg(unix)]
pub(crate) fn check_worktree_cleanup(cwd: &std::path::Path) -> Option<WorktreeCleanupWarning> {
    let repo_root = nori_config::GitRepo::discover(cwd)?.work_dir()?;

    // List worktrees (excluding main)
    let worktrees = codex_git::list_worktrees(&repo_root).ok()?;
    if worktrees.is_empty() {
        return None;
    }

    // Check disk space
    let disk_space = get_disk_space(&repo_root)?;

    evaluate_worktree_cleanup_warning(worktrees.len(), disk_space.used_percent)
}

/// Extract the worktree directory name from a path.
/// Returns the last path component if the parent directory is named `.worktrees`.
pub(crate) fn extract_worktree_name(dir: &std::path::Path) -> Option<String> {
    let parent = dir.parent()?;
    if parent.file_name()?.to_str()? == ".worktrees" {
        Some(dir.file_name()?.to_str()?.to_string())
    } else {
        None
    }
}

/// Check if the directory is a git worktree (not the main repository).
/// Returns true if this is a linked worktree, false if it's the main repo or not a git repo.
fn is_git_worktree(dir: Option<&std::path::Path>) -> bool {
    git_facts(dir).is_some_and(|facts| facts.is_linked_worktree())
}

/// The repository of `dir`, or of the current directory, opened in-process with `gix`.
fn git_repo(dir: Option<&std::path::Path>) -> Option<nori_config::GitRepo> {
    match dir {
        Some(dir) => nori_config::GitRepo::discover(dir),
        None => nori_config::GitRepo::discover(&env::current_dir().ok()?),
    }
}

/// The repository of `dir`, or of the current directory, read in-process with `gix`.
fn git_facts(dir: Option<&std::path::Path>) -> Option<nori_config::GitFacts> {
    match dir {
        Some(dir) => nori_config::GitFacts::read(dir),
        None => nori_config::GitFacts::read(&env::current_dir().ok()?),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::path::Path;
    use std::process::Stdio;

    fn run_git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git command should run");
        assert!(status.success(), "git {args:?} should succeed");
    }

    fn create_git_repo() -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().expect("temp dir should be created");
        run_git(dir.path(), &["init", "-b", "main"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "user.name", "Test User"]);
        std::fs::write(dir.path().join("file.txt"), "initial\n").expect("write file");
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-m", "initial"]);
        dir
    }

    #[test]
    fn test_parse_nori_version_skillsets_format() {
        // New format: "nori-skillsets 19.2.0"
        let version = parse_nori_version("nori-skillsets 19.2.0");
        assert_eq!(version, Some("19.2.0".to_string()));
    }

    #[test]
    fn nori_version_sources_use_skillset_terminology() {
        let skillsets = NoriVersionSource::Skillsets;
        let legacy_skillsets = NoriVersionSource::LegacySkillsets;

        assert_ne!(skillsets, legacy_skillsets);

        assert_eq!(skillsets.label(), "Skillsets");
        assert_eq!(legacy_skillsets.label(), "Legacy Skillsets");
    }

    #[test]
    fn test_get_git_branch_in_git_repo() {
        // This test runs in the actual git repo, so it should detect a branch
        // However, CI runners may checkout in detached HEAD state, so we only
        // verify format when a branch is detected, not that it must be present
        let branch = get_git_branch(None);
        if let Some(branch_name) = branch {
            assert!(!branch_name.is_empty(), "Branch name should not be empty");
            assert!(
                !branch_name.contains('\n'),
                "Branch name should not contain newlines"
            );
        }
        // If no branch is detected (detached HEAD, shallow clone), that's OK for CI
    }

    #[test]
    fn test_get_git_branch_truncates_long_names() {
        // Test that very long branch names are truncated
        // We'll implement truncation in get_git_branch
        // For now this tests that we handle the output correctly
        let branch = get_git_branch(None);
        if let Some(name) = branch {
            assert!(
                name.len() <= 30,
                "Branch names should be truncated to 30 characters"
            );
        }
    }

    #[test]
    fn test_collect_for_directory_uses_specified_path() {
        // Test that collect_for_directory runs git commands in the specified directory
        // We use the current repo directory which should be a valid git repo
        let current_dir = std::env::current_dir().expect("should have current dir");
        let info = SystemInfo::collect_for_directory(&current_dir);

        // The current directory is a git repo, so we should get branch info
        // (unless in detached HEAD state in CI)
        if let Some(branch) = &info.git_branch {
            assert!(!branch.is_empty());
        }
    }

    #[test]
    fn test_collect_for_directory_non_git_returns_none_branch() {
        // Test that a non-git directory returns None for git_branch
        let temp_dir = std::env::temp_dir();
        let info = SystemInfo::collect_for_directory(&temp_dir);

        // /tmp is not a git repo, so git_branch should be None
        assert!(
            info.git_branch.is_none(),
            "Non-git directory should have no branch"
        );
        assert!(
            !info.is_worktree,
            "Non-git directory should not be a worktree"
        );
    }

    #[test]
    fn seed_reports_the_branch_without_the_slow_lookups() {
        let repo = create_git_repo();
        // A file git would have to diff or stat if seeding did that work.
        std::fs::write(repo.path().join("untracked.txt"), "scratch\n").expect("write file");

        let seeded = SystemInfo::seed(repo.path());

        assert_eq!(seeded.git_branch.as_deref(), Some("main"));
        assert_eq!(
            (
                seeded.is_worktree,
                seeded.worktree_name,
                seeded.nori_version,
                seeded.active_skillsets,
                seeded.git_lines_added,
                seeded.git_lines_removed,
                seeded.git_has_untracked,
                seeded.worktree_cleanup_warning,
            ),
            (false, None, None, Vec::new(), None, None, false, None),
            "seeding must leave everything the background refresh collects at its default"
        );
    }

    #[test]
    fn seed_of_a_non_git_directory_is_empty() {
        let dir = tempfile::TempDir::new().expect("temp dir should be created");

        let seeded = SystemInfo::seed(dir.path());

        assert_eq!(seeded.git_branch, None);
        assert!(!seeded.is_worktree);
    }

    #[test]
    fn git_facts_read_from_files_match_what_git_reports() {
        let repo = create_git_repo();
        run_git(
            repo.path(),
            &["switch", "-c", "a-branch-name-that-is-longer-than-thirty"],
        );
        assert_eq!(
            get_git_branch(Some(repo.path())).as_deref(),
            Some("a-branch-name-that-is-longe...")
        );
        assert!(!is_git_worktree(Some(repo.path())));
        let nested = repo.path().join("src/deep");
        std::fs::create_dir_all(&nested).expect("nested dir");
        assert!(get_git_branch(Some(&nested)).is_some());

        let worktree = repo.path().join(".worktrees/wt");
        run_git(
            repo.path(),
            &[
                "worktree",
                "add",
                "-b",
                "wt-branch",
                worktree.to_str().expect("utf-8 path"),
            ],
        );
        assert!(is_git_worktree(Some(&worktree)));
        assert_eq!(
            get_git_branch(Some(&worktree)).as_deref(),
            Some("wt-branch")
        );

        run_git(&worktree, &["switch", "--detach"]);
        assert_eq!(get_git_branch(Some(&worktree)), None);
    }

    #[test]
    fn test_is_git_worktree_returns_false_for_non_git() {
        // Non-git directories should return false
        let temp_dir = std::env::temp_dir();
        assert!(
            !is_git_worktree(Some(&temp_dir)),
            "Non-git dir should not be worktree"
        );
    }

    #[test]
    fn test_get_git_stats_ignores_committed_branch_diff() {
        let dir = create_git_repo();
        run_git(dir.path(), &["switch", "-c", "feature"]);
        std::fs::write(dir.path().join("file.txt"), "initial\nfeature\n")
            .expect("write feature change");
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-m", "feature change"]);

        let stats = get_git_stats(Some(dir.path()));

        assert_eq!(stats, (None, None));
    }

    #[test]
    fn test_git_stats_do_not_count_untracked_file_lines() {
        let dir = create_git_repo();
        std::fs::write(dir.path().join("untracked.txt"), "one\ntwo\nthree\n")
            .expect("write untracked file");

        let stats = get_git_stats(Some(dir.path()));

        assert_eq!(stats, (None, None));
        assert!(has_untracked_files(Some(dir.path())));
    }

    #[test]
    fn test_parse_nori_version_with_program_name() {
        // Old format: "nori-ai 19.1.1"
        let version = parse_nori_version("nori-ai 19.1.1");
        assert_eq!(version, Some("19.1.1".to_string()));
    }

    #[test]
    fn test_parse_nori_version_version_only() {
        // Current format: "19.1.1"
        let version = parse_nori_version("19.1.1");
        assert_eq!(version, Some("19.1.1".to_string()));
    }

    #[test]
    fn test_parse_nori_version_with_newline() {
        // Real output has trailing newline
        let version = parse_nori_version("19.1.1\n");
        assert_eq!(version, Some("19.1.1".to_string()));
    }

    #[test]
    fn test_worktree_cleanup_warning_low_disk_with_worktrees() {
        let warning = evaluate_worktree_cleanup_warning(5, 94);
        assert!(
            warning.is_some(),
            "should warn when disk is 94% used and worktrees exist"
        );
        let w = warning.unwrap();
        assert_eq!(w.worktree_count, 5);
        assert_eq!(w.free_percent, 6);
    }

    #[test]
    fn test_worktree_cleanup_warning_sufficient_disk() {
        let warning = evaluate_worktree_cleanup_warning(5, 80);
        assert!(warning.is_none(), "should not warn when 20% free");
    }

    #[test]
    fn test_worktree_cleanup_warning_low_disk_no_worktrees() {
        let warning = evaluate_worktree_cleanup_warning(0, 95);
        assert!(warning.is_none(), "should not warn when no worktrees exist");
    }

    #[test]
    fn test_worktree_cleanup_warning_at_threshold() {
        let warning = evaluate_worktree_cleanup_warning(3, 90);
        assert!(
            warning.is_none(),
            "should not warn when exactly at 10% free (threshold)"
        );
    }

    #[test]
    fn test_worktree_cleanup_warning_just_below_threshold() {
        let warning = evaluate_worktree_cleanup_warning(3, 91);
        assert!(
            warning.is_some(),
            "should warn when 9% free (below threshold)"
        );
        let w = warning.unwrap();
        assert_eq!(w.free_percent, 9);
    }

    #[test]
    fn test_extract_worktree_name_from_worktrees_dir() {
        use std::path::Path;
        let path = Path::new("/home/user/repo/.worktrees/good-ash-20260205-204831");
        assert_eq!(
            extract_worktree_name(path),
            Some("good-ash-20260205-204831".to_string())
        );
    }

    #[test]
    fn test_extract_worktree_name_not_under_worktrees() {
        use std::path::Path;
        let path = Path::new("/home/user/repo/src/main");
        assert_eq!(extract_worktree_name(path), None);
    }

    #[test]
    fn test_extract_worktree_name_tmp_dir() {
        use std::path::Path;
        let path = Path::new("/tmp");
        assert_eq!(extract_worktree_name(path), None);
    }

    #[test]
    fn test_parse_active_skillsets_multiple_lines() {
        let output = "amol\nrust-dev\n";
        let result = parse_active_skillsets(output);
        assert_eq!(result, vec!["amol", "rust-dev"]);
    }

    #[test]
    fn test_parse_active_skillsets_single_line() {
        let output = "amol\n";
        let result = parse_active_skillsets(output);
        assert_eq!(result, vec!["amol"]);
    }

    #[test]
    fn test_parse_active_skillsets_empty() {
        let output = "";
        let result = parse_active_skillsets(output);
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_active_skillsets_trims_whitespace() {
        let output = "  amol  \n  rust-dev  \n";
        let result = parse_active_skillsets(output);
        assert_eq!(result, vec!["amol", "rust-dev"]);
    }

    #[test]
    fn test_parse_active_skillsets_skips_blank_lines() {
        let output = "amol\n\n\nrust-dev\n";
        let result = parse_active_skillsets(output);
        assert_eq!(result, vec!["amol", "rust-dev"]);
    }

    #[test]
    #[cfg(debug_assertions)]
    fn test_collect_sync_returns_valid_data() {
        // This test runs collect_sync and verifies the returned data is valid
        // Note: CI runners may checkout in detached HEAD state, so git_branch
        // may be None - we only verify format when it's present
        let info = SystemInfo::collect_sync();
        if let Some(branch) = &info.git_branch {
            assert!(!branch.is_empty(), "git_branch should not be empty if set");
            assert!(
                branch.len() <= 30,
                "git_branch should be truncated to 30 chars"
            );
        }
        // Other fields may or may not be populated depending on environment
    }

    #[test]
    fn used_percent_rounds_up_like_df() {
        assert_eq!(used_percent(1000, 100, 100), Some(90));
        assert_eq!(used_percent(1000, 999, 999), Some(1));
        assert_eq!(used_percent(1000, 1000, 1000), Some(0));
        assert_eq!(used_percent(0, 0, 0), None);
        // Blocks reserved for root count as used for the percentage but not as available.
        assert_eq!(used_percent(1000, 200, 100), Some(89));
    }

    /// On APFS `df` reports only the volume's own share (2% on a machine whose container is 99%
    /// full), so the reading is checked for plausibility rather than against `df`.
    #[cfg(unix)]
    #[test]
    fn disk_space_is_a_percentage_for_a_real_directory() {
        let used = get_disk_space(&std::env::temp_dir())
            .expect("statvfs")
            .used_percent;
        assert!((0..=100).contains(&used), "{used}% is not a percentage");
    }
}
