//! Language-server integration for Nori.

pub mod config;
mod convert;
pub mod diagnostics;
pub mod error;
pub mod launcher;
pub mod manager;
pub mod position;
pub mod registry;
pub mod render;
pub mod server;
pub mod tools;
pub mod types;

pub use helix_lsp::lsp;

pub use config::LspConfig;
pub use config::Timing;
pub use diagnostics::Diagnostic;
pub use diagnostics::DiagnosticsReport;
pub use diagnostics::FileDiagnostics;
pub use diagnostics::ReportStatus;
pub use diagnostics::Severity;
pub use error::LspError;
pub use error::LspResult;
pub use launcher::LaunchRequest;
pub use launcher::Launched;
pub use launcher::ProcessLauncher;
pub use launcher::ServerLauncher;
pub use manager::Answer;
pub use manager::LspManager;
pub use registry::Registry;
pub use registry::RootStrategy;
pub use registry::ServerConfig;
pub use registry::builtin_servers;
pub use registry::language_for_path;
pub use render::RenderOptions;
pub use render::render_summary;
pub use types::CallDirection;
pub use types::GotoKind;
pub use types::Target;
