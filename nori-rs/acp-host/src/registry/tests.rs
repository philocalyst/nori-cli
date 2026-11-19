use super::*;
use pretty_assertions::assert_eq;
use serial_test::serial;

#[test]
fn test_agent_slugs() {
    assert_eq!(AgentKind::ClaudeCode.slug(), "claude-code");
    assert_eq!(AgentKind::Codex.slug(), "codex");
    assert_eq!(AgentKind::Gemini.slug(), "gemini");
    assert_eq!(AgentKind::OpenCode.slug(), "opencode");
    assert_eq!(AgentKind::Antigravity.slug(), "antigravity");
}

#[test]
fn test_provider_slugs() {
    assert_eq!(Provider::Anthropic.slug(), "anthropic");
    assert_eq!(Provider::OpenAI.slug(), "openai");
    assert_eq!(Provider::Google.slug(), "google");
    assert_eq!(Provider::OpenCode.slug(), "opencode");
}

#[test]
fn test_agent_from_slug() {
    // Direct slugs
    assert_eq!(
        AgentKind::from_slug("claude-code"),
        Some(AgentKind::ClaudeCode)
    );
    assert_eq!(AgentKind::from_slug("codex"), Some(AgentKind::Codex));
    assert_eq!(AgentKind::from_slug("gemini"), Some(AgentKind::Gemini));
    assert_eq!(
        AgentKind::from_slug("antigravity"),
        Some(AgentKind::Antigravity)
    );
    assert_eq!(AgentKind::from_slug("agy"), Some(AgentKind::Antigravity));

    // Legacy aliases
    assert_eq!(
        AgentKind::from_slug("claude-acp"),
        Some(AgentKind::ClaudeCode)
    );
    assert_eq!(
        AgentKind::from_slug("claude-4.5"),
        Some(AgentKind::ClaudeCode)
    );
    assert_eq!(AgentKind::from_slug("codex-acp"), Some(AgentKind::Codex));
    assert_eq!(AgentKind::from_slug("gemini-acp"), Some(AgentKind::Gemini));
    assert_eq!(
        AgentKind::from_slug("gemini-2.5-flash"),
        Some(AgentKind::Gemini)
    );

    // Case insensitive
    assert_eq!(
        AgentKind::from_slug("Claude-Code"),
        Some(AgentKind::ClaudeCode)
    );
    assert_eq!(AgentKind::from_slug("CODEX"), Some(AgentKind::Codex));

    // Unknown
    assert_eq!(AgentKind::from_slug("unknown"), None);
}

#[test]
fn test_agent_provider_relationship() {
    assert_eq!(AgentKind::ClaudeCode.provider(), Provider::Anthropic);
    assert_eq!(AgentKind::Codex.provider(), Provider::OpenAI);
    assert_eq!(AgentKind::Gemini.provider(), Provider::Google);
}

#[test]
fn test_package_manager_commands() {
    assert_eq!(PackageManager::Npm.command(), "npx");
    assert_eq!(PackageManager::Bun.command(), "bunx");
}

#[test]
#[cfg(debug_assertions)]
fn test_get_mock_agent_config() {
    let config = get_agent_config("mock-model").expect("Should return config for mock-model");

    assert_eq!(config.provider_slug, "mock-acp");
    assert!(
        config.command.contains("mock_acp_agent"),
        "Command should contain 'mock_acp_agent', got: {}",
        config.command
    );
    assert_eq!(config.args, Vec::<String>::new());
    assert_eq!(config.provider_info.name, "Mock ACP");
    assert_eq!(config.provider_info.request_max_retries, 1);
    assert_eq!(config.provider_info.stream_max_retries, 1);
}

#[test]
#[cfg(debug_assertions)]
fn test_get_mock_agent_alt_config() {
    let config =
        get_agent_config("mock-model-alt").expect("Should return config for mock-model-alt");

    assert_eq!(config.provider_slug, "mock-acp-alt");
    assert!(
        config.command.contains("mock_acp_agent"),
        "Command should contain 'mock_acp_agent', got: {}",
        config.command
    );
    assert_eq!(config.args, Vec::<String>::new());
    assert_eq!(config.provider_info.name, "Mock ACP Alt");
}

#[test]
fn test_get_claude_code_config() {
    let config = get_agent_config("claude-code").expect("Should return config for claude-code");

    assert_eq!(config.provider_slug, "claude-code");
    assert_eq!(config.agent, AgentKind::ClaudeCode);
    // Command should be npx or bunx
    assert!(
        config.command == "npx" || config.command == "bunx",
        "Command should be npx or bunx, got: {}",
        config.command
    );
    assert!(
        config
            .args
            .contains(&"@agentclientprotocol/claude-agent-acp@latest".to_string())
    );
    assert_eq!(config.provider_info.name, "Claude Code ACP");
}

#[test]
fn test_get_codex_config() {
    let config = get_agent_config("codex").expect("Should return config for codex");

    assert_eq!(config.provider_slug, "codex");
    assert_eq!(config.agent, AgentKind::Codex);
    assert!(
        config.command == "npx" || config.command == "bunx",
        "Command should be npx or bunx, got: {}",
        config.command
    );
    // Uses the actively maintained Agent Client Protocol adapter.
    assert_eq!(
        config.args,
        vec!["@agentclientprotocol/codex-acp@latest".to_string()]
    );
    let codex_config = config
        .env
        .get("CODEX_CONFIG")
        .and_then(|config| serde_json::from_str::<serde_json::Value>(config).ok())
        .expect("Codex config should be valid JSON");
    assert_eq!(codex_config["features"]["goals"], false);
    assert_eq!(codex_config["approval_policy"], "never");
    assert_eq!(codex_config["sandbox_mode"], "danger-full-access");
    assert_eq!(config.provider_info.name, "Codex ACP");
}

#[test]
#[serial]
fn opencode_uses_its_own_identity_and_upstream_fallback() {
    reset_registry();
    let config = get_agent_config("opencode").expect("OpenCode config");
    assert_eq!(config.agent, AgentKind::OpenCode);
    assert_eq!(config.display_name, "OpenCode");
    assert_eq!(config.command, "opencode");
    assert_eq!(config.args, vec!["acp"]);
    assert_eq!(config.model_injection, ModelInjection::None);
}

#[test]
fn codex_config_preserves_existing_settings_while_disabling_native_goals() {
    let config = codex_config_for_nori(Some(
        r#"{"model":"gpt-test","approval_policy":"on-request","sandbox_mode":"read-only","default_permissions":"custom","features":{"goals":true,"other":true}}"#,
    ))
    .expect("valid CODEX_CONFIG should merge");

    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&config).expect("merged JSON"),
        serde_json::json!({
            "model": "gpt-test",
            "approval_policy": "never",
            "sandbox_mode": "danger-full-access",
            "features": {
                "goals": false,
                "other": true
            }
        })
    );
}

#[test]
fn codex_config_rejects_invalid_ambient_json() {
    let error = codex_config_for_nori(Some("not-json"))
        .expect_err("invalid CODEX_CONFIG should remain visible");

    assert!(
        error
            .to_string()
            .contains("CODEX_CONFIG must be valid JSON")
    );
}

#[test]
fn claude_injects_model_via_anthropic_model_env() {
    let mut config = get_agent_config("claude-code").expect("claude config");

    config
        .inject_model("claude-opus-4-6")
        .expect("model injection should succeed");

    assert_eq!(
        config.env.get("ANTHROPIC_MODEL").map(String::as_str),
        Some("claude-opus-4-6"),
        "the spawned Claude subprocess must receive the model via ANTHROPIC_MODEL"
    );
}

#[test]
fn gemini_injects_model_via_gemini_model_env() {
    let mut config = get_agent_config("gemini").expect("gemini config");

    config
        .inject_model("gemini-2.5-pro")
        .expect("model injection should succeed");

    assert_eq!(
        config.env.get("GEMINI_MODEL").map(String::as_str),
        Some("gemini-2.5-pro"),
        "the spawned Gemini subprocess must receive the model via GEMINI_MODEL"
    );
}

#[test]
fn codex_injects_model_into_codex_config_preserving_goals_flag() {
    let mut config = get_agent_config("codex").expect("codex config");

    config
        .inject_model("gpt-5.1-codex")
        .expect("model injection should succeed");

    let codex_config = config
        .env
        .get("CODEX_CONFIG")
        .expect("CODEX_CONFIG should be present for codex");
    let parsed: serde_json::Value =
        serde_json::from_str(codex_config).expect("CODEX_CONFIG should be valid JSON");
    assert_eq!(
        parsed.get("model").and_then(serde_json::Value::as_str),
        Some("gpt-5.1-codex"),
        "the model must be merged into CODEX_CONFIG"
    );
    assert_eq!(
        parsed.pointer("/features/goals"),
        Some(&serde_json::Value::Bool(false)),
        "injecting a model must not clobber the goals-disabled flag"
    );
}

#[test]
#[serial]
fn custom_agent_env_override_injects_model() {
    reset_registry();
    use nori_config::AgentConfigToml;
    use nori_config::AgentDistributionToml;
    use nori_config::LocalDistribution;
    use nori_config::ModelOverrideToml;
    let custom = AgentConfigToml {
        name: "Custom Env".to_string(),
        slug: "custom-env".to_string(),
        distribution: AgentDistributionToml {
            local: Some(LocalDistribution {
                command: "/usr/bin/agent".to_string(),
                args: vec!["acp".to_string()],
                env: std::collections::HashMap::new(),
            }),
            ..Default::default()
        },
        context_window_size: None,
        auth_hint: None,
        transcript_base_dir: None,
        model_override: Some(ModelOverrideToml {
            env: Some("MY_MODEL".to_string()),
            arg: None,
        }),
    };
    initialize_registry(vec![custom]).unwrap();

    let mut config = get_agent_config("custom-env").expect("custom-env config");
    config.inject_model("some-model").expect("inject");

    assert_eq!(
        config.env.get("MY_MODEL").map(String::as_str),
        Some("some-model"),
        "a BYO agent declaring an env override must receive the model there"
    );
    reset_registry();
}

#[test]
#[serial]
fn custom_agent_arg_override_appends_model_flag() {
    reset_registry();
    use nori_config::AgentConfigToml;
    use nori_config::AgentDistributionToml;
    use nori_config::LocalDistribution;
    use nori_config::ModelOverrideToml;
    let custom = AgentConfigToml {
        name: "Custom Arg".to_string(),
        slug: "custom-arg".to_string(),
        distribution: AgentDistributionToml {
            local: Some(LocalDistribution {
                command: "/usr/bin/agent".to_string(),
                args: vec!["acp".to_string()],
                env: std::collections::HashMap::new(),
            }),
            ..Default::default()
        },
        context_window_size: None,
        auth_hint: None,
        transcript_base_dir: None,
        model_override: Some(ModelOverrideToml {
            env: None,
            arg: Some("--model".to_string()),
        }),
    };
    initialize_registry(vec![custom]).unwrap();

    let mut config = get_agent_config("custom-arg").expect("custom-arg config");
    config.inject_model("some-model").expect("inject");

    assert_eq!(
        config.args,
        vec!["acp", "--model", "some-model"],
        "a BYO agent declaring an arg override must receive the model as a CLI flag"
    );
    reset_registry();
}

#[test]
#[serial]
fn custom_agent_without_override_leaves_spawn_config_unchanged() {
    reset_registry();
    use nori_config::AgentConfigToml;
    use nori_config::AgentDistributionToml;
    use nori_config::LocalDistribution;
    let custom = AgentConfigToml {
        name: "Custom None".to_string(),
        slug: "custom-none".to_string(),
        distribution: AgentDistributionToml {
            local: Some(LocalDistribution {
                command: "/usr/bin/agent".to_string(),
                args: vec!["acp".to_string()],
                env: std::collections::HashMap::from([("K".to_string(), "v".to_string())]),
            }),
            ..Default::default()
        },
        context_window_size: None,
        auth_hint: None,
        transcript_base_dir: None,
        model_override: None,
    };
    initialize_registry(vec![custom]).unwrap();

    let mut config = get_agent_config("custom-none").expect("custom-none config");
    let env_before = config.env.clone();
    let args_before = config.args.clone();
    config
        .inject_model("some-model")
        .expect("inject should be a no-op");

    assert_eq!(
        config.env, env_before,
        "an agent with no model channel must not have its env mutated"
    );
    assert_eq!(
        config.args, args_before,
        "an agent with no model channel must not have its args mutated"
    );
    reset_registry();
}

#[test]
fn test_get_gemini_config() {
    let config = get_agent_config("gemini").expect("Should return config for gemini");

    assert_eq!(config.provider_slug, "gemini");
    assert_eq!(config.agent, AgentKind::Gemini);
    assert!(
        config.command == "npx" || config.command == "bunx",
        "Command should be npx or bunx, got: {}",
        config.command
    );
    assert_eq!(config.args, ["@google/gemini-cli", "--acp"]);
    assert_eq!(config.provider_info.name, "Gemini ACP");
}

#[test]
fn test_legacy_model_names() {
    // Claude legacy names
    assert!(get_agent_config("claude-acp").is_ok());
    assert!(get_agent_config("claude-4.5").is_ok());

    // Codex legacy names
    assert!(get_agent_config("codex-acp").is_ok());

    // Gemini legacy names
    assert!(get_agent_config("gemini-acp").is_ok());
    assert!(get_agent_config("gemini-2.5-flash").is_ok());
}

#[test]
fn test_get_unknown_model_returns_error() {
    let result = get_agent_config("unknown-model-xyz");

    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(err_msg.contains("unknown-model-xyz"));
}

#[test]
fn test_get_agent_config_normalizes_model_names() {
    // Should work with lowercase model names
    assert!(
        get_agent_config("claude-code").is_ok(),
        "Lowercase 'claude-code' should work"
    );

    // Should work with mixed case (normalized to lowercase)
    let claude_result = get_agent_config("Claude-Code");
    assert!(
        claude_result.is_ok(),
        "Mixed case 'Claude-Code' should work"
    );
    assert_eq!(
        claude_result.unwrap().provider_slug,
        "claude-code",
        "Should resolve to claude-code provider"
    );

    // Should still reject unknown models
    let unknown_result = get_agent_config("unknown-model-xyz");
    assert!(unknown_result.is_err(), "Unknown model should return error");
    let err_msg = unknown_result.unwrap_err().to_string();
    assert!(
        err_msg.contains("unknown-model-xyz"),
        "Error message should contain original input"
    );
}

#[test]
#[serial]
#[cfg(debug_assertions)]
fn test_list_available_agents_debug_build() {
    reset_registry();
    let binary = tempfile::NamedTempFile::new().expect("mock executable fixture");
    let agents = list_available_agents_with_mock_binary(Some(binary.path()));
    assert_eq!(
        agents.len(),
        7,
        "Available mock binary adds two fixture agents"
    );

    let names: Vec<&str> = agents.iter().map(|a| a.display_name.as_str()).collect();
    assert!(names.contains(&"Mock ACP"), "Should have Mock ACP");
    assert!(names.contains(&"Mock ACP Alt"), "Should have Mock ACP Alt");
    assert!(names.contains(&"Claude Code"), "Should have Claude Code");
    assert!(names.contains(&"Codex"), "Should have Codex");
    assert!(names.contains(&"Gemini"), "Should have Gemini");
    assert!(names.contains(&"OpenCode"), "Should have OpenCode");
    assert!(names.contains(&"Antigravity"), "Should have Antigravity");
    let absent = binary.path().with_extension("missing");
    let without_mock = list_available_agents_with_mock_binary(Some(&absent));
    assert_eq!(
        without_mock.len(),
        5,
        "Missing mock binary hides both fixture agents"
    );
    assert!(
        without_mock
            .iter()
            .all(|agent| !agent.agent_name.starts_with("mock-model"))
    );
}

#[test]
fn test_list_available_agents_contains_production_agents() {
    let agents = list_available_agents();
    let names: Vec<&str> = agents.iter().map(|a| a.display_name.as_str()).collect();

    // Production agents should always be present
    assert!(names.contains(&"Claude Code"), "Should have Claude Code");
    assert!(names.contains(&"Codex"), "Should have Codex");
    assert!(names.contains(&"Gemini"), "Should have Gemini");
}

#[test]
fn test_auth_hint_returns_actionable_instructions() {
    // Claude Code should mention `/login` for instructions
    let claude_hint = AgentKind::ClaudeCode.auth_hint();
    assert!(
        claude_hint.contains("/login"),
        "Claude hint should mention '/login', got: {claude_hint}"
    );

    // Codex should mention `/login` to authenticate
    let codex_hint = AgentKind::Codex.auth_hint();
    assert!(
        codex_hint.contains("/login"),
        "Codex hint should mention '/login', got: {codex_hint}"
    );

    // Gemini should mention `/login` for instructions
    let gemini_hint = AgentKind::Gemini.auth_hint();
    assert!(
        gemini_hint.contains("/login"),
        "Gemini hint should mention '/login', got: {gemini_hint}"
    );
}

#[test]
fn test_agent_transcript_base_dir() {
    // Each agent should return the correct base directory path relative to home
    assert_eq!(
        AgentKind::ClaudeCode.transcript_base_dir(),
        ".claude/projects"
    );
    assert_eq!(AgentKind::Codex.transcript_base_dir(), ".codex/sessions");
    assert_eq!(AgentKind::Gemini.transcript_base_dir(), ".gemini/tmp");
}

// ========================================================================
// Data-driven registry tests
// ========================================================================

/// Reset the global registry so tests don't interfere with each other.
fn reset_registry() {
    let mut registry = AGENT_REGISTRY.write().unwrap();
    *registry = None;
}

#[test]
fn test_build_default_agents_returns_five_builtins() {
    let agents = build_default_agents();
    assert_eq!(agents.len(), 5);
    let slugs: Vec<&str> = agents.iter().map(|a| a.slug.as_str()).collect();
    assert!(slugs.contains(&"claude-code"));
    assert!(slugs.contains(&"codex"));
    assert!(slugs.contains(&"gemini"));
    assert!(slugs.contains(&"opencode"));
    assert!(slugs.contains(&"antigravity"));
}

#[test]
fn test_build_default_agents_have_correct_names() {
    let agents = build_default_agents();
    let claude = agents.iter().find(|a| a.slug == "claude-code").unwrap();
    assert_eq!(claude.name, "Claude Code");
    let codex = agents.iter().find(|a| a.slug == "codex").unwrap();
    assert_eq!(codex.name, "Codex");
    let gemini = agents.iter().find(|a| a.slug == "gemini").unwrap();
    assert_eq!(gemini.name, "Gemini");
}

#[test]
fn test_build_default_agents_have_agent_kind() {
    let agents = build_default_agents();
    for agent in &agents {
        assert!(
            agent.kind.is_some(),
            "Built-in agent {} should have AgentKind",
            agent.slug
        );
    }
}

#[test]
fn test_build_registry_with_no_custom_agents() {
    let registry = build_registry(vec![]).unwrap();
    assert_eq!(registry.len(), 5);
}

#[test]
fn test_build_registry_appends_custom_agent() {
    use nori_config::AgentConfigToml;
    use nori_config::AgentDistributionToml;
    use nori_config::PackageDistribution;
    let custom = AgentConfigToml {
        name: "Kimi".to_string(),
        slug: "kimi".to_string(),
        distribution: AgentDistributionToml {
            uvx: Some(PackageDistribution {
                package: "kimi-cli".to_string(),
                args: vec!["acp".to_string()],
            }),
            ..Default::default()
        },
        context_window_size: None,
        auth_hint: None,
        transcript_base_dir: None,
        model_override: None,
    };
    let registry = build_registry(vec![custom]).unwrap();
    assert_eq!(registry.len(), 6);
    let kimi = registry.iter().find(|a| a.slug == "kimi").unwrap();
    assert_eq!(kimi.name, "Kimi");
    assert!(kimi.kind.is_none());
}

#[test]
fn test_build_registry_custom_overrides_builtin() {
    use nori_config::AgentConfigToml;
    use nori_config::AgentDistributionToml;
    use nori_config::LocalDistribution;
    let custom_claude = AgentConfigToml {
        name: "My Claude".to_string(),
        slug: "claude-code".to_string(),
        distribution: AgentDistributionToml {
            local: Some(LocalDistribution {
                command: "/my/claude".to_string(),
                args: vec![],
                env: std::collections::HashMap::new(),
            }),
            ..Default::default()
        },
        context_window_size: Some(300_000),
        auth_hint: Some("Use my auth".to_string()),
        transcript_base_dir: None,
        model_override: None,
    };
    let registry = build_registry(vec![custom_claude]).unwrap();
    // The override replaces the built-in entry rather than adding another.
    assert_eq!(registry.len(), 5);
    let claude = registry.iter().find(|a| a.slug == "claude-code").unwrap();
    assert_eq!(claude.name, "My Claude");
    assert_eq!(claude.context_window_size, Some(300_000));
}

#[test]
fn test_build_registry_rejects_duplicate_custom_slugs() {
    use nori_config::AgentConfigToml;
    use nori_config::AgentDistributionToml;
    use nori_config::PackageDistribution;
    let agents = vec![
        AgentConfigToml {
            name: "Agent A".to_string(),
            slug: "my-agent".to_string(),
            distribution: AgentDistributionToml {
                npx: Some(PackageDistribution {
                    package: "pkg-a".to_string(),
                    args: vec![],
                }),
                ..Default::default()
            },
            context_window_size: None,
            auth_hint: None,
            transcript_base_dir: None,
            model_override: None,
        },
        AgentConfigToml {
            name: "Agent B".to_string(),
            slug: "my-agent".to_string(),
            distribution: AgentDistributionToml {
                npx: Some(PackageDistribution {
                    package: "pkg-b".to_string(),
                    args: vec![],
                }),
                ..Default::default()
            },
            context_window_size: None,
            auth_hint: None,
            transcript_base_dir: None,
            model_override: None,
        },
    ];
    let result = build_registry(agents);
    assert!(result.is_err());
}

#[test]
#[serial]
fn test_get_agent_config_resolves_custom_uvx_agent() {
    reset_registry();
    use nori_config::AgentConfigToml;
    use nori_config::AgentDistributionToml;
    use nori_config::PackageDistribution;
    let custom = AgentConfigToml {
        name: "Kimi".to_string(),
        slug: "kimi".to_string(),
        distribution: AgentDistributionToml {
            uvx: Some(PackageDistribution {
                package: "kimi-cli".to_string(),
                args: vec!["acp".to_string()],
            }),
            ..Default::default()
        },
        context_window_size: None,
        auth_hint: None,
        transcript_base_dir: None,
        model_override: None,
    };
    initialize_registry(vec![custom]).unwrap();
    let config = get_agent_config("kimi").expect("should find kimi");
    assert_eq!(config.command, "uvx");
    assert_eq!(config.args, vec!["kimi-cli", "acp"]);
    assert_eq!(config.provider_slug, "kimi");
    reset_registry();
}

#[test]
#[serial]
fn test_get_agent_config_resolves_custom_local_agent() {
    reset_registry();
    use nori_config::AgentConfigToml;
    use nori_config::AgentDistributionToml;
    use nori_config::LocalDistribution;
    let custom = AgentConfigToml {
        name: "Local Agent".to_string(),
        slug: "local-test".to_string(),
        distribution: AgentDistributionToml {
            local: Some(LocalDistribution {
                command: "/usr/bin/my-agent".to_string(),
                args: vec!["--mode".to_string(), "acp".to_string()],
                env: std::collections::HashMap::from([("KEY".to_string(), "val".to_string())]),
            }),
            ..Default::default()
        },
        context_window_size: None,
        auth_hint: Some("Set KEY env var".to_string()),
        transcript_base_dir: None,
        model_override: None,
    };
    initialize_registry(vec![custom]).unwrap();
    let config = get_agent_config("local-test").expect("should find local-test");
    assert_eq!(config.command, "/usr/bin/my-agent");
    assert_eq!(config.args, vec!["--mode", "acp"]);
    assert_eq!(config.env.get("KEY").unwrap(), "val");
    assert_eq!(config.auth_hint, "Set KEY env var");
    reset_registry();
}

#[test]
#[serial]
fn test_list_available_agents_includes_custom() {
    reset_registry();
    use nori_config::AgentConfigToml;
    use nori_config::AgentDistributionToml;
    use nori_config::PackageDistribution;
    let custom = AgentConfigToml {
        name: "Kimi".to_string(),
        slug: "kimi".to_string(),
        distribution: AgentDistributionToml {
            uvx: Some(PackageDistribution {
                package: "kimi-cli".to_string(),
                args: vec![],
            }),
            ..Default::default()
        },
        context_window_size: None,
        auth_hint: None,
        transcript_base_dir: None,
        model_override: None,
    };
    initialize_registry(vec![custom]).unwrap();
    let agents = list_available_agents();
    assert!(
        agents.iter().any(|a| a.display_name.as_str() == "Kimi"),
        "Should contain custom agent Kimi"
    );
    reset_registry();
}

#[test]
#[serial]
fn test_get_agent_display_name_custom_agent() {
    reset_registry();
    use nori_config::AgentConfigToml;
    use nori_config::AgentDistributionToml;
    use nori_config::PackageDistribution;
    let custom = AgentConfigToml {
        name: "My Custom Agent".to_string(),
        slug: "my-custom".to_string(),
        distribution: AgentDistributionToml {
            npx: Some(PackageDistribution {
                package: "my-custom-pkg".to_string(),
                args: vec![],
            }),
            ..Default::default()
        },
        context_window_size: None,
        auth_hint: None,
        transcript_base_dir: None,
        model_override: None,
    };
    initialize_registry(vec![custom]).unwrap();
    assert_eq!(get_agent_display_name("my-custom"), "My Custom Agent");
    reset_registry();
}

#[test]
fn test_claude_code_context_window_is_1m() {
    assert_eq!(AgentKind::ClaudeCode.context_window_size(), 1_000_000);
}

#[test]
fn test_clean_stale_acp_cache_removes_zed_entries() {
    let cache_dir = tempfile::tempdir().unwrap();
    let zed_scope = cache_dir.path().join("@zed-industries");
    std::fs::create_dir_all(zed_scope.join("claude-agent-acp@0.23.1@@@1")).unwrap();
    std::fs::create_dir_all(zed_scope.join("claude-agent-acp@0.20.0@@@1")).unwrap();
    std::fs::create_dir_all(zed_scope.join("codex-acp@0.5.0@@@1")).unwrap();
    // Unrelated package under the same scope should be preserved
    std::fs::create_dir_all(zed_scope.join("some-other-package@1.0.0@@@1")).unwrap();

    let acp_scope = cache_dir.path().join("@agentclientprotocol");
    std::fs::create_dir_all(acp_scope.join("claude-agent-acp@0.70.0@@@1")).unwrap();
    std::fs::create_dir_all(acp_scope.join("codex-acp@0.10.0@@@1")).unwrap();

    clean_stale_acp_cache(cache_dir.path());

    // Stale @zed-industries entries should be gone
    assert!(!zed_scope.join("claude-agent-acp@0.23.1@@@1").exists());
    assert!(!zed_scope.join("claude-agent-acp@0.20.0@@@1").exists());
    assert!(!zed_scope.join("codex-acp@0.5.0@@@1").exists());
    // Unrelated package should be preserved
    assert!(zed_scope.join("some-other-package@1.0.0@@@1").exists());
    // @agentclientprotocol entries should be untouched
    assert!(acp_scope.join("claude-agent-acp@0.70.0@@@1").exists());
    assert!(acp_scope.join("codex-acp@0.10.0@@@1").exists());
}

#[test]
fn test_clean_stale_acp_cache_noop_when_no_zed_scope() {
    let cache_dir = tempfile::tempdir().unwrap();
    let acp_scope = cache_dir.path().join("@agentclientprotocol");
    std::fs::create_dir_all(acp_scope.join("claude-agent-acp@0.70.0@@@1")).unwrap();

    // Should not panic or error when @zed-industries doesn't exist
    clean_stale_acp_cache(cache_dir.path());

    assert!(acp_scope.join("claude-agent-acp@0.70.0@@@1").exists());
}

#[test]
fn test_clean_stale_acp_cache_noop_when_cache_dir_missing() {
    let cache_dir = std::path::PathBuf::from("/tmp/nonexistent-bun-cache-test-dir");
    // Should not panic or error
    clean_stale_acp_cache(&cache_dir);
}

#[test]
fn test_agent_config_includes_auth_hint() {
    // Get config for claude-code and verify it has an auth hint with /login
    let config = get_agent_config("claude-code").expect("Should return config");
    assert!(
        !config.auth_hint.is_empty(),
        "Config should have a non-empty auth_hint"
    );
    assert!(
        config.auth_hint.contains("/login"),
        "Claude config auth_hint should mention '/login', got: {}",
        config.auth_hint
    );

    // Get config for codex and verify it has an auth hint with /login
    let config = get_agent_config("codex").expect("Should return config");
    assert!(
        !config.auth_hint.is_empty(),
        "Codex config should have a non-empty auth_hint"
    );
    assert!(
        config.auth_hint.contains("/login"),
        "Codex config auth_hint should mention '/login', got: {}",
        config.auth_hint
    );

    // Get config for gemini and verify it has an auth hint with /login
    let config = get_agent_config("gemini").expect("Should return config");
    assert!(
        !config.auth_hint.is_empty(),
        "Gemini config should have a non-empty auth_hint"
    );
    assert!(
        config.auth_hint.contains("/login"),
        "Gemini config auth_hint should mention '/login', got: {}",
        config.auth_hint
    );
}

#[test]
fn antigravity_is_the_official_cli_and_takes_no_model_at_start() {
    assert_eq!(
        AgentKind::from_slug("agy"),
        Some(AgentKind::Antigravity),
        "agy alias should resolve"
    );
    assert_eq!(AgentKind::Antigravity.context_window_size(), 1_000_000);
    assert_eq!(AgentKind::Antigravity.provider(), Provider::Google);
    assert_eq!(
        AgentKind::Antigravity.model_injection(),
        ModelInjection::None
    );
    assert!(
        AgentKind::Antigravity
            .auth_hint()
            .contains("agy acp --login")
    );
}
