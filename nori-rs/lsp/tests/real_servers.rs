//! Smoke tests against real language servers on `PATH` (rust-analyzer and clangd), on throwaway
//! projects. Ignored by default: run them with
//! `cargo test -p nori-lsp --test real_servers -- --ignored --nocapture`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use nori_lsp::LspConfig;
use nori_lsp::LspManager;
use nori_lsp::ReportStatus;
use nori_lsp::Target;
use nori_lsp::Timing;
use pretty_assertions::assert_eq;

#[tokio::test]
#[ignore = "needs rust-analyzer on PATH and takes a while"]
async fn rust_analyzer_reports_a_type_error_and_finds_a_definition() {
    if which::which("rust-analyzer").is_err() {
        eprintln!("rust-analyzer is not on PATH; skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"smoke\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    let lib = dir.path().join("src/lib.rs");
    let good = "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n";
    std::fs::write(&lib, good).unwrap();

    let config = LspConfig {
        timing: Timing {
            first_diagnostics: Duration::from_secs(60),
            ready_wait: Duration::from_secs(60),
            ..Timing::default()
        },
        ..LspConfig::default()
    };
    let manager = LspManager::new(dir.path(), config);

    let broken = format!(
        "{good}\npub fn use_it() -> i32 {{\n    let x: i32 = \"nope\";\n    add(x, 1)\n}}\n"
    );
    std::fs::write(&lib, &broken).unwrap();
    manager
        .notify_file_changed(&lib, broken.clone())
        .await
        .unwrap();
    let report = manager.diagnostics_after_edit(&lib).await;
    eprintln!("{report:#?}");
    assert_eq!(report.status, ReportStatus::Fresh);
    assert!(report.error_count() >= 1);

    let answer = manager
        .definition(&Target::new(&lib, 6).symbol("add"))
        .await
        .unwrap();
    eprintln!("{answer:#?}");
    assert_eq!(answer.value[0].range.line, 1);

    manager.shutdown().await;
}

#[tokio::test]
#[ignore = "needs clangd on PATH"]
async fn clangd_reports_an_error_and_finds_a_definition() {
    if which::which("clangd").is_err() {
        eprintln!("clangd is not on PATH; skipping");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("compile_flags.txt"), "-xc\n").unwrap();
    let source = dir.path().join("main.c");
    let good = "int add(int a, int b) { return a + b; }\n";
    std::fs::write(&source, good).unwrap();

    let config = LspConfig {
        timing: Timing {
            first_diagnostics: Duration::from_secs(20),
            ready_wait: Duration::from_secs(20),
            ..Timing::default()
        },
        ..LspConfig::default()
    };
    let manager = LspManager::new(dir.path(), config);
    let broken = format!(
        "{good}int main(void) {{\n    int x = undefined_name;\n    return add(x, 1);\n}}\n"
    );
    std::fs::write(&source, &broken).unwrap();
    manager.notify_file_changed(&source, broken).await.unwrap();
    let report = manager.diagnostics_after_edit(&source).await;
    eprintln!("{report:#?}");
    assert_eq!(report.status, ReportStatus::Fresh);
    assert!(report.error_count() >= 1);

    let answer = manager
        .definition(&Target::new(&source, 4).symbol("add"))
        .await
        .unwrap();
    eprintln!("{answer:#?}");
    assert_eq!(answer.value[0].range.line, 1);
    manager.shutdown().await;
}
