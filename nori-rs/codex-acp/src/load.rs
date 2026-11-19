//! The Codex configuration an in-process agent starts with.

use std::path::PathBuf;

use codex_core::config::ConfigBuilder;
use codex_core::config::ConfigOverrides;
use codex_utils_cli::CliConfigOverrides;

use crate::codex_agent::CodexAgent;

/// Load Codex's configuration, with `overrides` (`codex -c` pairs) on top, and build the agent.
///
/// Nori's sessions always use its full-access, no-approval policy: Nori owns approvals, and its
/// explicit overrides take precedence over a resumed thread's saved profile. The host-wide
/// configuration (`/etc/codex/config.toml` and managed policy) is ignored: it is written for
/// whichever Codex the machine has installed, and the one linked into this process may be older and
/// fail to parse it.
pub(crate) async fn load_agent(
    codex_home: Option<PathBuf>,
    overrides: Vec<String>,
) -> std::io::Result<CodexAgent> {
    let cli_kv_overrides = CliConfigOverrides {
        raw_overrides: overrides,
    }
    .parse_overrides()
    .map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("error parsing -c overrides: {e}"),
        )
    })?;

    let harness_overrides = ConfigOverrides {
        approval_policy: Some(codex_protocol::protocol::AskForApproval::Never),
        sandbox_mode: Some(codex_protocol::config_types::SandboxMode::DangerFullAccess),
        ..ConfigOverrides::default()
    };

    // Point the host-wide files at a path that does not exist, so they load as empty.
    let nowhere = std::env::temp_dir().join("nori-codex-no-host-config");
    let mut builder = ConfigBuilder::default()
        .cli_overrides(cli_kv_overrides)
        .harness_overrides(harness_overrides)
        .loader_overrides(codex_config::LoaderOverrides {
            system_config_path: Some(nowhere.join("config.toml")),
            system_requirements_path: Some(nowhere.join("requirements.toml")),
            managed_config_path: Some(nowhere.join("managed_config.toml")),
            ignore_managed_requirements: true,
            ..Default::default()
        });
    if let Some(codex_home) = codex_home {
        builder = builder.codex_home(codex_home);
    }
    let mut config = builder.build().await.map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("error loading config: {e}"),
        )
    })?;

    // Code Mode carries Nori's host tools, so it is on unless the user configured it themselves.
    let explicitly_configured = config
        .config_layer_stack
        .effective_config()
        .get("features")
        .and_then(|features| features.get("code_mode"))
        .is_some();
    let mut candidate = config.features.get().clone();
    candidate.enable(codex_features::Feature::CodeMode);
    if config.features.can_set(&candidate).is_ok() {
        if !explicitly_configured {
            drop(config.features.set(candidate));
        }
        config
            .runtime_feature_defaults
            .insert(codex_features::Feature::CodeMode, true);
    }
    // Apply residency requirement so the HTTP client sends the
    // x-openai-internal-codex-residency header on all requests.
    codex_login::default_client::set_default_client_residency_requirement(
        config.enforce_residency.value(),
    );

    CodexAgent::new(config).await
}
