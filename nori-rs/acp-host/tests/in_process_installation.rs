//! A built-in agent counts as installed when this build registers its in-process launcher, even
//! when the CLI it replaced is nowhere on `PATH`.

use std::sync::Arc;

use nori_acp_host::in_process::Registration;
use nori_acp_host::in_process::register;
use nori_acp_host::registry::AcpAgentInfo;
use nori_acp_host::registry::AgentKind;

#[test]
fn built_in_agents_are_installed_without_their_cli_on_path() {
    let empty_path = tempfile::tempdir().expect("temp dir");
    let previous_path = std::env::var_os("PATH");

    // This binary runs a single test, so the process-wide change cannot race another one.
    unsafe { std::env::set_var("PATH", empty_path.path()) };

    for (name, agent) in [
        ("codex", AgentKind::Codex),
        ("claude", AgentKind::ClaudeCode),
    ] {
        register(Registration {
            name,
            launch: Arc::new(|_, _, _| Box::pin(async { Ok(()) })),
            prewarm: None,
            resolve: None,
        });

        let info = AcpAgentInfo::from_agent(agent);

        assert!(
            info.is_installed,
            "{} should be installed through its in-process launcher",
            info.display_name
        );
        assert_eq!(info.managed_by, None);
    }

    if let Some(path) = previous_path {
        unsafe { std::env::set_var("PATH", path) };
    }
}
