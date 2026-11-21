//! Text for the model: the `<diagnostic_summary>` block that follows an edit, and the shared
//! helpers that print paths and diagnostics.

use std::fmt::Write as _;
use std::path::Path;
use std::path::PathBuf;

use crate::diagnostics::Diagnostic;
use crate::diagnostics::DiagnosticsReport;
use crate::diagnostics::FileDiagnostics;
use crate::diagnostics::ReportStatus;
use crate::diagnostics::Severity;
use crate::diagnostics::count;

/// How much of a report is printed. Errors are always listed before warnings and the caps keep one
/// bad edit from flooding the context.
#[derive(Debug, Clone)]
pub struct RenderOptions {
    /// Paths are printed relative to this directory when they lie under it.
    pub root: PathBuf,
    pub max_file_errors: i32,
    pub max_file_warnings: i32,
    pub max_project_files: i32,
    pub max_errors_per_project_file: i32,
}

impl RenderOptions {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            max_file_errors: 20,
            max_file_warnings: 8,
            max_project_files: 5,
            max_errors_per_project_file: 3,
        }
    }
}

/// `path` relative to `root` when it is inside it, else as given.
pub fn display_path(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

const MAX_MESSAGE_CHARS: usize = 300;

fn one_line(message: &str) -> String {
    let flat = message.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= MAX_MESSAGE_CHARS {
        return flat;
    }
    let cut: String = flat.chars().take(MAX_MESSAGE_CHARS).collect();
    format!("{cut}...")
}

fn describe(diagnostic: &Diagnostic) -> String {
    let origin = diagnostic.source.as_deref().unwrap_or(&diagnostic.server);
    let origin = match &diagnostic.code {
        Some(code) => format!("{origin} {code}"),
        None => origin.to_owned(),
    };
    format!(
        "{} {}:{} [{origin}] {}",
        diagnostic.severity.label(),
        diagnostic.range.line,
        diagnostic.range.column,
        one_line(&diagnostic.message)
    )
}

fn plural(count: i32, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

fn totals(diagnostics: &[Diagnostic]) -> String {
    let errors = count(diagnostics, Severity::Error);
    let warnings = count(diagnostics, Severity::Warning);
    format!(
        "{}, {}",
        plural(errors, "error"),
        plural(warnings, "warning")
    )
}

fn list_capped(out: &mut String, diagnostics: &[Diagnostic], severity: Severity, cap: i32) {
    let wanted: Vec<&Diagnostic> = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == severity)
        .collect();
    let shown = usize::try_from(cap).unwrap_or(0);
    for diagnostic in wanted.iter().take(shown) {
        let _ = writeln!(out, "  {}", describe(diagnostic));
    }
    if wanted.len() > shown {
        let _ = writeln!(
            out,
            "  ... and {} more {}s not shown",
            wanted.len() - shown,
            severity.label()
        );
    }
}

fn render_project(out: &mut String, project: &[FileDiagnostics], options: &RenderOptions) {
    if project.is_empty() {
        return;
    }
    let _ = writeln!(out, "New errors in other files:");
    let shown_files = usize::try_from(options.max_project_files).unwrap_or(0);
    for file in project.iter().take(shown_files) {
        let errors = count(&file.diagnostics, Severity::Error);
        let _ = writeln!(
            out,
            "{}: {}",
            display_path(&file.path, &options.root),
            plural(errors, "error")
        );
        list_capped(
            out,
            &file.diagnostics,
            Severity::Error,
            options.max_errors_per_project_file,
        );
    }
    if project.len() > shown_files {
        let _ = writeln!(
            out,
            "... and {} more files with new errors",
            project.len() - shown_files
        );
    }
}

/// The model-facing block for a report, or an empty string when no server serves the file (there
/// is nothing to say). Errors come first; counts are capped; positions are 1-based `line:col`.
pub fn render_summary(report: &DiagnosticsReport, options: &RenderOptions) -> String {
    let mut body = String::new();
    let path = display_path(&report.path, &options.root);
    match &report.status {
        ReportStatus::NoServer => return String::new(),
        ReportStatus::Failed(reason) => {
            let servers = report.servers.join(", ");
            let _ = writeln!(
                body,
                "Language server {servers} is not running ({reason}); no diagnostics for {path}."
            );
        }
        ReportStatus::Fresh | ReportStatus::Stale => {
            if report.file.is_empty() {
                let _ = writeln!(body, "{path}: no errors or warnings");
            } else {
                let _ = writeln!(body, "{path}: {}", totals(&report.file));
                list_capped(
                    &mut body,
                    &report.file,
                    Severity::Error,
                    options.max_file_errors,
                );
                list_capped(
                    &mut body,
                    &report.file,
                    Severity::Warning,
                    options.max_file_warnings,
                );
            }
            if report.status == ReportStatus::Stale {
                let _ = writeln!(
                    body,
                    "(the language server had not reported on this change yet; these are its last results)"
                );
            }
        }
    }
    render_project(&mut body, &report.project, options);
    format!("<diagnostic_summary>\n{body}</diagnostic_summary>")
}

/// A listing of one file's diagnostics (errors, warnings, then the rest), for `lsp_diagnostics`.
pub fn render_file_list(
    path: &Path,
    diagnostics: &[Diagnostic],
    options: &RenderOptions,
) -> String {
    let name = display_path(path, &options.root);
    if diagnostics.is_empty() {
        return format!("{name}: no errors or warnings");
    }
    let mut out = format!("{name}: {}\n", totals(diagnostics));
    list_capped(
        &mut out,
        diagnostics,
        Severity::Error,
        options.max_file_errors,
    );
    list_capped(
        &mut out,
        diagnostics,
        Severity::Warning,
        options.max_file_warnings,
    );
    let other = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity > Severity::Warning)
        .count();
    if other > 0 {
        let _ = writeln!(out, "  ({other} information/hint diagnostics not shown)");
    }
    out.trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::types::Range;

    fn diag(severity: Severity, line: i32, column: i32, message: &str) -> Diagnostic {
        Diagnostic {
            severity,
            range: Range {
                line,
                column,
                end_line: line,
                end_column: column + 1,
            },
            message: message.to_owned(),
            source: Some("rustc".to_owned()),
            code: Some("E0308".to_owned()),
            server: "rust-analyzer".to_owned(),
        }
    }

    fn report(
        file: Vec<Diagnostic>,
        project: Vec<FileDiagnostics>,
        status: ReportStatus,
    ) -> DiagnosticsReport {
        DiagnosticsReport {
            path: PathBuf::from("/repo/src/a.rs"),
            file,
            project,
            servers: vec!["rust-analyzer".to_owned()],
            status,
        }
    }

    #[test]
    fn renders_errors_before_warnings_with_one_based_positions() {
        let file = vec![
            diag(Severity::Error, 12, 5, "mismatched types\nexpected `i32`"),
            diag(Severity::Warning, 3, 1, "unused variable"),
        ];
        let text = render_summary(
            &report(file, Vec::new(), ReportStatus::Fresh),
            &RenderOptions::new("/repo"),
        );
        assert_eq!(
            text,
            "<diagnostic_summary>\n\
             src/a.rs: 1 error, 1 warning\n  \
             error 12:5 [rustc E0308] mismatched types expected `i32`\n  \
             warning 3:1 [rustc E0308] unused variable\n\
             </diagnostic_summary>"
        );
    }

    #[test]
    fn caps_the_listing_and_says_what_was_dropped() {
        let file: Vec<Diagnostic> = (1..=25)
            .map(|n| diag(Severity::Error, n, 1, "boom"))
            .collect();
        let mut options = RenderOptions::new("/repo");
        options.max_file_errors = 3;
        let text = render_summary(&report(file, Vec::new(), ReportStatus::Fresh), &options);
        assert!(text.contains("src/a.rs: 25 errors, 0 warnings"));
        assert!(text.contains("... and 22 more errors not shown"));
        assert_eq!(text.matches("boom").count(), 3);
    }

    #[test]
    fn project_errors_are_a_separate_capped_section() {
        let other = |name: &str| FileDiagnostics {
            path: PathBuf::from(format!("/repo/src/{name}.rs")),
            diagnostics: vec![diag(Severity::Error, 1, 1, "broken")],
        };
        let mut options = RenderOptions::new("/repo");
        options.max_project_files = 1;
        let text = render_summary(
            &report(
                Vec::new(),
                vec![other("b"), other("c")],
                ReportStatus::Fresh,
            ),
            &options,
        );
        assert!(text.contains("src/a.rs: no errors or warnings"));
        assert!(text.contains("New errors in other files:\nsrc/b.rs: 1 error"));
        assert!(text.contains("... and 1 more files with new errors"));
        assert!(!text.contains("src/c.rs: 1 error"));
    }

    #[test]
    fn stale_failed_and_missing_servers() {
        let options = RenderOptions::new("/repo");
        let stale = render_summary(
            &report(Vec::new(), Vec::new(), ReportStatus::Stale),
            &options,
        );
        assert!(stale.contains("had not reported on this change yet"));
        let failed = render_summary(
            &report(
                Vec::new(),
                Vec::new(),
                ReportStatus::Failed("exited with status 1".to_owned()),
            ),
            &options,
        );
        assert!(failed.contains("rust-analyzer is not running (exited with status 1)"));
        let none =
            DiagnosticsReport::without_server(PathBuf::from("/repo/x.txt"), ReportStatus::NoServer);
        assert_eq!(render_summary(&none, &options), "");
    }
}
