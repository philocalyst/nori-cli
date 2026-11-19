//! The agents built into this binary. Each agent crate exposes one `registration()`; this is the
//! list of them, behind the features that include them.

use nori_acp_host::in_process;

/// Make every built-in agent this build has available to the ACP host.
pub fn register() {
    #[cfg(feature = "native-codex")]
    in_process::register(nori_codex_acp::registration());
    #[cfg(feature = "native-claude")]
    in_process::register(nori_claude_acp::registration());
    #[cfg(feature = "native-opencode")]
    in_process::register(nori_opencode_acp::registration());
    #[cfg(feature = "native-gemini")]
    in_process::register(nori_gemini_acp::registration());
    #[cfg(feature = "native-antigravity-sdk")]
    in_process::register(nori_antigravity_acp::registration());
}
