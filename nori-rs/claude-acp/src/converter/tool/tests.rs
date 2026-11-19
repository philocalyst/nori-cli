use super::*;
use serde_json::json;

#[test]
fn test_extract_read_tool_info() {
    let input = json!({"file_path": "/path/to/file.rs"});
    let info = extract_tool_info("Read", &input, None);

    assert_eq!(info.kind, ToolKind::Read);
    assert!(info.title.contains("file.rs"));
    assert!(info.locations.is_some());
}

#[test]
fn test_extract_bash_tool_info() {
    // Reference: vendors/claude-code-acp/src/tools.ts:97-111
    // TypeScript always uses command as title, description goes to content[]
    let input = json!({
        "command": "cargo build",
        "description": "Build the project"
    });
    let info = extract_tool_info("Bash", &input, None);

    assert_eq!(info.kind, ToolKind::Execute);
    // Title should be the command, not the description
    assert_eq!(info.title, "`cargo build`");
}

#[test]
fn test_extract_bash_tool_info_no_description() {
    // Reference: vendors/claude-code-acp/src/tools.ts:97-111
    // TypeScript wraps command in backticks: `command`
    let input = json!({"command": "cargo test --release"});
    let info = extract_tool_info("Bash", &input, None);

    assert_eq!(info.kind, ToolKind::Execute);
    assert_eq!(info.title, "`cargo test --release`");
}

#[test]
fn test_extract_bash_tool_info_empty_command() {
    // When command is empty, show "Terminal"
    let input = json!({});
    let info = extract_tool_info("Bash", &input, None);

    assert_eq!(info.kind, ToolKind::Execute);
    assert_eq!(info.title, "Terminal");
}

#[test]
fn test_extract_bash_tool_info_with_backticks() {
    // Backticks in command should be escaped
    let input = json!({"command": "echo `date`"});
    let info = extract_tool_info("Bash", &input, None);

    assert_eq!(info.kind, ToolKind::Execute);
    assert_eq!(info.title, "`echo \\`date\\``");
}

#[test]
fn test_extract_grep_tool_info() {
    let input = json!({"pattern": "fn main"});
    let info = extract_tool_info("Grep", &input, None);

    assert_eq!(info.kind, ToolKind::Search);
    assert!(info.title.contains("fn main"));
}

#[test]
fn test_extract_mcp_tool_info() {
    let input = json!({});
    let info = extract_tool_info("mcp__server__custom_tool", &input, None);

    assert_eq!(info.kind, ToolKind::Other);
    assert!(info.title.contains("custom_tool"));
}

#[test]
fn nori_host_tools_are_titled_by_their_own_name() {
    let info = extract_tool_info("mcp__nori__git_commit", &json!({}), None);
    assert_eq!(
        (info.title.as_str(), info.kind),
        ("git_commit", ToolKind::Other)
    );
}

#[test]
fn test_extract_acp_bash_tool_info() {
    // mcp__acp__Bash should display like Bash
    let input = json!({"command": "tree -L 2 -d"});
    let info = extract_tool_info("mcp__acp__Bash", &input, None);

    assert_eq!(info.kind, ToolKind::Execute);
    assert!(info.title.contains("tree"));
    assert!(!info.title.contains("MCP")); // Should NOT show "MCP: Bash"
}

#[test]
fn test_extract_acp_read_tool_info() {
    // mcp__acp__Read should display like Read
    let input = json!({"file_path": "/path/to/file.rs"});
    let info = extract_tool_info("mcp__acp__Read", &input, None);

    assert_eq!(info.kind, ToolKind::Read);
    assert!(info.title.contains("Read"));
    assert!(info.title.contains("file.rs"));
    assert!(!info.title.contains("MCP"));
}

#[test]
fn test_truncate_long_path() {
    let long_path = "/very/long/path/to/some/deeply/nested/directory/structure/file.rs";
    let truncated = truncate_path(long_path, None);
    assert!(truncated.len() <= 60 || truncated == "file.rs");
}

#[test]
fn test_truncate_string() {
    assert_eq!(truncate_string("short", 10), "short");
    assert_eq!(truncate_string("this is a longer string", 10), "this is...");
}

#[test]
fn test_extract_ls_tool_info() {
    let input = json!({"path": "/path/to/directory"});
    let info = extract_tool_info("LS", &input, None);

    assert_eq!(info.kind, ToolKind::Search);
    assert!(info.title.contains("List"));
    assert!(info.title.contains("directory"));
}

#[test]
fn test_extract_ls_tool_info_current_dir() {
    let input = json!({"path": "."});
    let info = extract_tool_info("LS", &input, None);

    assert_eq!(info.kind, ToolKind::Search);
    assert!(info.title.contains("List"));
    assert!(info.title.contains('.'));
}

#[test]
fn test_extract_read_tool_info_with_line_range() {
    // TypeScript uses 1-indexed format: "Read file.rs (101 - 150)"
    // Reference: vendors/claude-code-acp/src/tools.ts:207-227
    let input = json!({
        "file_path": "/path/to/file.rs",
        "offset": 100,
        "limit": 50
    });
    let info = extract_tool_info("Read", &input, None);

    assert_eq!(info.kind, ToolKind::Read);
    assert!(info.title.contains("Read"));
    assert!(info.title.contains("file.rs"));
    assert!(info.title.contains("101 - 150")); // 1-indexed: (100+1) - (100+50)
}

#[test]
fn test_extract_read_tool_info_with_offset_only() {
    // TypeScript uses 1-indexed display
    let input = json!({
        "file_path": "/path/to/file.rs",
        "offset": 200
    });
    let info = extract_tool_info("Read", &input, None);

    assert_eq!(info.kind, ToolKind::Read);
    assert!(info.title.contains("Read"));
    assert!(info.title.contains("file.rs"));
    assert!(info.title.contains("from line 201")); // 1-indexed: 200+1
}

#[test]
fn test_extract_read_tool_info_no_range() {
    let input = json!({"file_path": "/path/to/file.rs"});
    let info = extract_tool_info("Read", &input, None);

    assert_eq!(info.kind, ToolKind::Read);
    assert!(info.title.contains("Read"));
    assert!(info.title.contains("file.rs"));
    assert!(!info.title.contains("lines"));
    assert!(!info.title.contains("from line"));
}

#[test]
fn test_extract_acp_ls_tool_info() {
    // mcp__acp__LS should display like LS
    let input = json!({"path": "/Volumes/soddy/git_workspace/project"});
    let info = extract_tool_info("mcp__acp__LS", &input, None);

    assert_eq!(info.kind, ToolKind::Search);
    assert!(info.title.contains("List"));
    assert!(info.title.contains("project"));
    assert!(!info.title.contains("MCP")); // Should NOT show "MCP: LS"
}

#[test]
fn test_extract_read_tool_info_limit_zero() {
    // Edge case: limit=0 should show "from line X" instead of range
    // TypeScript uses 1-indexed display
    let input = json!({
        "file_path": "/path/to/file.rs",
        "offset": 100,
        "limit": 0
    });
    let info = extract_tool_info("Read", &input, None);

    assert_eq!(info.kind, ToolKind::Read);
    assert!(info.title.contains("Read"));
    assert!(info.title.contains("from line 101")); // 1-indexed: 100+1
    assert!(!info.title.contains(" - ")); // Should NOT show range
}

#[test]
fn test_extract_read_tool_info_overflow_protection() {
    // Test saturating arithmetic with very large values
    let input = json!({
        "file_path": "/path/to/file.rs",
        "offset": u64::MAX - 10,
        "limit": 100
    });
    let info = extract_tool_info("Read", &input, None);

    assert_eq!(info.kind, ToolKind::Read);
    // Should not panic due to overflow
    assert!(info.title.contains("Read"));
    assert!(info.title.contains("file.rs"));
}

#[test]
fn test_extract_read_tool_info_offset_zero() {
    // TypeScript uses 1-indexed display: offset=0 shows (1 - 10)
    let input = json!({
        "file_path": "/path/to/file.rs",
        "offset": 0,
        "limit": 10
    });
    let info = extract_tool_info("Read", &input, None);

    assert_eq!(info.kind, ToolKind::Read);
    assert!(info.title.contains("Read"));
    assert!(info.title.contains("1 - 10")); // 1-indexed: (0+1) - (0+10)
}

#[test]
fn test_extract_read_tool_info_with_cwd_file_in_root() {
    // Test file directly in cwd shows ./ prefix
    let cwd = PathBuf::from("/Volumes/soddy/git_workspace/claude-code-acp-rs");
    let input = json!({"file_path": "/Volumes/soddy/git_workspace/claude-code-acp-rs/Cargo.toml"});
    let info = extract_tool_info("Read", &input, Some(&cwd));

    assert_eq!(info.kind, ToolKind::Read);
    assert!(info.title.contains("./Cargo.toml"));
    assert!(!info.title.contains("/Volumes/"));
}

#[test]
fn test_extract_read_tool_info_with_cwd_file_in_subdir() {
    // Test file in subdirectory shows relative path without ./
    let cwd = PathBuf::from("/Volumes/soddy/git_workspace/claude-code-acp-rs");
    let input = json!({"file_path": "/Volumes/soddy/git_workspace/claude-code-acp-rs/src/main.rs"});
    let info = extract_tool_info("Read", &input, Some(&cwd));

    assert_eq!(info.kind, ToolKind::Read);
    assert!(info.title.contains("src/main.rs"));
    assert!(!info.title.contains("./"));
    assert!(!info.title.contains("/Volumes/"));
}

#[test]
fn test_extract_read_tool_info_with_cwd_file_outside() {
    // Test file outside cwd shows absolute path
    let cwd = PathBuf::from("/Volumes/soddy/git_workspace/claude-code-acp-rs");
    let input = json!({"file_path": "/tmp/other-file.txt"});
    let info = extract_tool_info("Read", &input, Some(&cwd));

    assert_eq!(info.kind, ToolKind::Read);
    assert!(info.title.contains("/tmp/other-file.txt"));
    assert!(!info.title.contains("./"));
}

#[test]
fn test_extract_ls_tool_info_with_cwd() {
    // Test LS tool with cwd
    let cwd = PathBuf::from("/Volumes/soddy/git_workspace/project");
    let input = json!({"path": "/Volumes/soddy/git_workspace/project/src"});
    let info = extract_tool_info("LS", &input, Some(&cwd));

    assert_eq!(info.kind, ToolKind::Search);
    assert!(info.title.contains("src"));
    assert!(!info.title.contains("/Volumes/"));
}

#[test]
fn test_extract_edit_tool_info_with_cwd() {
    // Test Edit tool with cwd
    let cwd = PathBuf::from("/home/user/project");
    let input = json!({"file_path": "/home/user/project/src/lib.rs"});
    let info = extract_tool_info("Edit", &input, Some(&cwd));

    assert_eq!(info.kind, ToolKind::Edit);
    assert!(info.title.contains("src/lib.rs"));
    assert!(!info.title.contains("/home/"));
}

#[test]
fn test_truncate_path_with_cwd_long_relative() {
    // Test that long relative paths are still truncated
    let cwd = PathBuf::from("/a/b/c");
    let long_path = "/a/b/c/very/deep/nested/directory/structure/that/goes/on/and/on/file.txt";
    let result = truncate_path(long_path, Some(&cwd));

    // Should show relative path (shorter than MAX_DISPLAY_LENGTH) or just filename if truncated
    if result.len() > MAX_DISPLAY_LENGTH {
        assert_eq!(result, "file.txt");
    } else {
        assert!(result.contains("very") || result.contains("file.txt"));
    }
}

#[test]
fn test_clean_path_removes_dot_slash_prefix() {
    // Note: clean_path() no longer strips ./ prefix to preserve it when
    // intentionally added by truncate_path(). Use strip_prefix("./") directly
    // if you need to remove it.
    assert_eq!(clean_path("./src"), "./src");
    assert_eq!(clean_path("./Cargo.toml"), "./Cargo.toml");
    // Edge case: just "./" is preserved
    assert_eq!(clean_path("./"), "./");
}

#[test]
fn test_clean_path_removes_duplicate_slashes() {
    assert_eq!(clean_path("src//file.rs"), "src/file.rs");
    assert_eq!(clean_path("/path//to//file.rs"), "/path/to/file.rs");
    assert_eq!(clean_path("///three///slashes"), "/three/slashes");
}

#[test]
fn test_clean_path_combined() {
    // clean_path() preserves ./ prefix but handles duplicate slashes
    assert_eq!(clean_path("./src//file.rs"), "./src/file.rs");
    // .//path becomes ./path (the // after ./ becomes /)
    assert_eq!(clean_path(".//path//to//file.rs"), "./path/to/file.rs");
}

#[test]
fn test_extract_glob_tool_with_dot_slash() {
    // Test that Glob patterns with ./ prefix are cleaned
    let input = json!({"pattern": "./src/**/*.rs"});
    let info = extract_tool_info("Glob", &input, None);

    assert_eq!(info.kind, ToolKind::Search);
    assert_eq!(info.title, "Find: `src/**/*.rs`");
    assert!(!info.title.contains("./"));
}

#[test]
fn test_extract_glob_tool_with_double_slashes() {
    // Test that Glob patterns with double slashes are cleaned
    let input = json!({"pattern": "src//**/*.rs"});
    let info = extract_tool_info("Glob", &input, None);

    assert_eq!(info.kind, ToolKind::Search);
    assert_eq!(info.title, "Find: `src/**/*.rs`");
    assert!(!info.title.contains("//"));
}

#[test]
fn test_extract_glob_tool_combined_cleaning() {
    // Test that both ./ prefix and double slashes are cleaned
    let input = json!({"pattern": ".//src//**//*.rs"});
    let info = extract_tool_info("Glob", &input, None);

    assert_eq!(info.kind, ToolKind::Search);
    // .//src becomes /src after stripping ./ and then cleaning //
    assert_eq!(info.title, "Find: `/src/**/*.rs`");
}

#[test]
fn test_truncate_path_with_double_slash_and_cwd() {
    // Test that truncate_path also handles double slashes
    let cwd = PathBuf::from("/project");
    let input = json!({"file_path": "/project//src//lib.rs"});
    let info = extract_tool_info("Read", &input, Some(&cwd));

    assert_eq!(info.kind, ToolKind::Read);
    assert!(info.title.contains("src/lib.rs"));
    assert!(!info.title.contains("//"));
}

#[test]
fn test_extract_glob_tool_simple_pattern() {
    // Test that simple patterns work correctly
    let input = json!({"pattern": "*.rs"});
    let info = extract_tool_info("Glob", &input, None);

    assert_eq!(info.kind, ToolKind::Search);
    assert_eq!(info.title, "Find: `*.rs`");
}

#[test]
fn test_extract_glob_tool_complex_pattern() {
    // Test that complex patterns work correctly
    let input = json!({"pattern": "src/**/*.rs"});
    let info = extract_tool_info("Glob", &input, None);

    assert_eq!(info.kind, ToolKind::Search);
    assert_eq!(info.title, "Find: `src/**/*.rs`");
}

#[test]
fn test_clean_path_many_duplicate_slashes() {
    // Test that many duplicate slashes are handled correctly
    assert_eq!(clean_path("a//////////////b"), "a/b");
    assert_eq!(clean_path("path////to////file.rs"), "path/to/file.rs");
    assert_eq!(clean_path("///a///b///c///"), "/a/b/c/");
}

#[test]
fn test_clean_path_parent_directory_with_slash() {
    // Test parent directory with double slash
    assert_eq!(clean_path("..//file.rs"), "../file.rs");
    assert_eq!(clean_path("../..//file.rs"), "../../file.rs");
}

#[test]
fn test_clean_path_redundant_current_dir() {
    // Test redundant current directory references
    // Note: clean_path() replaces ././ with ./ (preserves single ./)
    assert_eq!(clean_path("././file.rs"), "./file.rs");
    // .//file has double slash after ./, becomes ./file
    assert_eq!(clean_path("././/file.rs"), "./file.rs");
}

#[test]
fn test_clean_path_empty_string() {
    // Test empty string
    assert_eq!(clean_path(""), "");
}

#[test]
fn test_clean_path_preserves_single_slash() {
    // Test that single slashes are preserved
    assert_eq!(clean_path("/usr/local/bin"), "/usr/local/bin");
    assert_eq!(clean_path("C:/Program Files/"), "C:/Program Files/");
}

#[test]
fn test_truncate_path_with_many_slashes_and_cwd() {
    // Test that truncate_path handles many duplicate slashes correctly
    let cwd = PathBuf::from("/project");
    let result = truncate_path("/project////src////lib.rs", Some(&cwd));

    assert_eq!(result, "src/lib.rs");
    assert!(!result.contains("//"));
}

#[test]
fn test_extract_read_tool_info_with_many_slashes() {
    // Test Read tool with many duplicate slashes in path
    let cwd = PathBuf::from("/project");
    let input = json!({"file_path": "/project////src////lib.rs"});
    let info = extract_tool_info("Read", &input, Some(&cwd));

    assert_eq!(info.kind, ToolKind::Read);
    assert!(info.title.contains("src/lib.rs"));
    assert!(!info.title.contains("//"));
}

// ============================================================
// Performance benchmarks (simple benchmarks without criterion)
// ============================================================

#[test]
fn benchmark_clean_path_simple() {
    // Benchmark simple path cleaning (no duplicates)
    let iterations = 10_000;
    let start = std::time::Instant::now();

    for _ in 0..iterations {
        std::hint::black_box(clean_path("src/file.rs"));
    }

    let elapsed = start.elapsed();
    #[allow(clippy::cast_sign_loss)]
    let per_iter_ns = elapsed.as_nanos() / iterations as u128;

    // Should be very fast (< 1000ns per iteration)
    assert!(
        per_iter_ns < 1000,
        "clean_path too slow: {per_iter_ns}ns per iteration"
    );
}

#[test]
fn benchmark_clean_path_with_duplicates() {
    // Benchmark path cleaning with duplicate slashes
    let iterations = 10_000;
    let start = std::time::Instant::now();

    for _ in 0..iterations {
        std::hint::black_box(clean_path("src////to////file.rs"));
    }

    let elapsed = start.elapsed();
    #[allow(clippy::cast_sign_loss)]
    let per_iter_ns = elapsed.as_nanos() / iterations as u128;

    // Should still be fast (< 2000ns per iteration)
    assert!(
        per_iter_ns < 2000,
        "clean_path with duplicates too slow: {per_iter_ns}ns per iteration"
    );
}

#[test]
fn benchmark_clean_path_long() {
    // Benchmark long path cleaning
    let long_path = "a/b/c/d/e/f/g/h/i/j/k/l/m/n/o/p/q/r/s/t/u/v/w/x/y/z/file.rs";
    let iterations = 10_000;
    let start = std::time::Instant::now();

    for _ in 0..iterations {
        std::hint::black_box(clean_path(long_path));
    }

    let elapsed = start.elapsed();
    #[allow(clippy::cast_sign_loss)]
    let per_iter_ns = elapsed.as_nanos() / iterations as u128;

    // Longer paths may be slower but still should be reasonable
    assert!(
        per_iter_ns < 5000,
        "clean_path long path too slow: {per_iter_ns}ns per iteration"
    );
}

#[test]
fn test_extract_slash_command_tool_info() {
    let input = json!({"command": "commit", "args": "-m 'fix bug'"});
    let info = extract_tool_info("SlashCommand", &input, None);

    assert_eq!(info.kind, ToolKind::Other);
    assert!(info.title.contains("/commit"));
}

#[test]
fn test_extract_slash_command_tool_info_empty() {
    let input = json!({});
    let info = extract_tool_info("SlashCommand", &input, None);

    assert_eq!(info.kind, ToolKind::Other);
    assert!(info.title.contains("Slash command"));
}

#[test]
fn test_extract_skill_tool_info() {
    let input = json!({"skill": "pdf", "args": "document.pdf"});
    let info = extract_tool_info("Skill", &input, None);

    assert_eq!(info.kind, ToolKind::Other);
    assert!(info.title.contains("pdf"));
}

#[test]
fn test_extract_skill_tool_info_empty() {
    let input = json!({});
    let info = extract_tool_info("Skill", &input, None);

    assert_eq!(info.kind, ToolKind::Other);
    assert!(info.title.contains("Skill"));
}
