//! Nori's side of Rex, the terminal multiplexer it often runs in.
//!
//! Rex tells every program it starts where it runs (`REX_SESSION`, `REX_BLOCK`) and how to reach
//! its server (`REX_BIN_DIR/rex`, the CLI shipped with that server). [`RexPane`] is that pane.
//! [`subscribe`] follows the pane's own events through `rex events`, which is the server's
//! public, versioned stream, and turns the ones Nori acts on into [`PaneEvent`]s:
//!
//! - a click on a link Nori printed (`url_clicked`): Rex never opens a link itself; it reports
//!   the click to the clients and to anyone following the pane, with mouse reporting off, so the
//!   terminal's own selection keeps working. Found empirically and documented in
//!   `docs/terminal-integration.md`.
//!
//! The subscription reconnects with backoff when the server or the CLI goes away, says so
//! ([`PaneEvent::Lost`], then [`PaneEvent::Following`] again), and ends for good when the pane
//! or its session closes, or when the receiver is dropped.

mod events;

pub use events::PaneEvent;
pub use events::subscribe;

use std::path::PathBuf;

/// The Rex pane this process runs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RexPane {
    /// The `rex` CLI of the server that runs the pane.
    pub rex: PathBuf,
    pub session: String,
    pub block: String,
}

impl RexPane {
    /// The pane, when Nori runs in one and the CLI is there to reach its server.
    pub fn from_env() -> Option<Self> {
        let session = std::env::var("REX_SESSION")
            .ok()
            .filter(|v| !v.is_empty())?;
        let block = std::env::var("REX_BLOCK").ok().filter(|v| !v.is_empty())?;
        // The CLI next to the server speaks its version of the API; one on PATH may not.
        let beside_server = std::env::var_os("REX_BIN_DIR")
            .map(|dir| PathBuf::from(dir).join("rex"))
            .filter(|path| path.is_file());
        let on_path = || {
            std::env::var_os("PATH").and_then(|path| {
                std::env::split_paths(&path)
                    .map(|dir| dir.join("rex"))
                    .find(|candidate| candidate.is_file())
            })
        };
        let rex = beside_server.or_else(on_path)?;
        Some(Self {
            rex,
            session,
            block,
        })
    }
}
