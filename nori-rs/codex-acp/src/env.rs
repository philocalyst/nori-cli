//! The environment a custom Codex adapter would have read, as explicit overrides.
//!
//! The host calls [`fold_environment`] with the environment configured for the agent, because a
//! process shared with other agents has no per-agent environment: `CODEX_CONFIG` (JSON) becomes
//! `codex -c` overrides, and Nori's full-access policy is enforced last so nothing in an
//! account's configuration can relax it.

use std::collections::HashMap;

use nori_acp_kit::InProcessSpec;

/// Policy keys Nori owns; configuration for them is dropped before the enforced values go in.
const OWNED_KEYS: [&str; 4] = [
    "approval_policy",
    "sandbox_mode",
    "default_permissions",
    "permission_profile",
];

/// `spec` with the environment's Codex configuration folded into its overrides and full access
/// enforced.
pub(crate) fn fold_environment(
    mut spec: InProcessSpec,
    env: &HashMap<String, String>,
) -> InProcessSpec {
    spec.environment.extend(env.clone());
    spec.config.extend(codex_overrides_from_env(env));
    spec.config.retain(|pair| {
        !pair
            .split_once('=')
            .is_some_and(|(key, _)| OWNED_KEYS.contains(&key.trim()))
    });
    spec.config.push("approval_policy=\"never\"".to_owned());
    spec.config
        .push("sandbox_mode=\"danger-full-access\"".to_owned());
    spec
}

/// The `CODEX_CONFIG` environment variable an adapter would read, as the `key=value` overrides an
/// in-process Codex takes: nested objects become dotted keys (`{"features":{"goals":false}}` is
/// `features.goals=false`).
fn codex_overrides_from_env(env: &HashMap<String, String>) -> Vec<String> {
    let Some(json) = env
        .get("CODEX_CONFIG")
        .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
    else {
        return Vec::new();
    };
    let mut overrides = Vec::new();
    flatten(&mut overrides, String::new(), &json);
    overrides
}

fn flatten(out: &mut Vec<String>, prefix: String, value: &serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                let key = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten(out, key, value);
            }
        }
        // A JSON string, number or boolean is also a valid TOML one, which is what `-c` parses.
        scalar if !prefix.is_empty() => out.push(format!("{prefix}={scalar}")),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn codex_config_json_becomes_dotted_overrides() {
        let env = HashMap::from([(
            "CODEX_CONFIG".to_owned(),
            r#"{"model":"o3","features":{"goals":false},"model_reasoning_effort":"high"}"#
                .to_owned(),
        )]);
        let mut overrides = codex_overrides_from_env(&env);
        overrides.sort();
        assert_eq!(
            overrides,
            vec![
                "features.goals=false",
                "model=\"o3\"",
                "model_reasoning_effort=\"high\"",
            ]
        );
        assert!(codex_overrides_from_env(&HashMap::new()).is_empty());
    }

    #[test]
    fn native_codex_enforces_full_access_after_account_and_environment_overrides() {
        let spec = InProcessSpec {
            agent: "codex".into(),
            home: Some("/account".into()),
            config: vec![
                "model=\"gpt-example\"".into(),
                "approval_policy=\"on-request\"".into(),
                "default_permissions=\"custom\"".into(),
            ],
            environment: HashMap::new(),
        };
        let env = HashMap::from([(
            "CODEX_CONFIG".to_owned(),
            r#"{"sandbox_mode":"read-only","model_reasoning_effort":"high"}"#.to_owned(),
        )]);
        let resolved = fold_environment(spec.clone(), &env);
        assert_eq!(resolved.home, spec.home);
        assert!(
            resolved
                .config
                .contains(&"model=\"gpt-example\"".to_owned())
        );
        assert!(
            resolved
                .config
                .contains(&"model_reasoning_effort=\"high\"".to_owned())
        );
        assert_eq!(
            &resolved.config[resolved.config.len() - 2..],
            &[
                "approval_policy=\"never\"",
                "sandbox_mode=\"danger-full-access\""
            ]
        );
        assert!(
            !resolved
                .config
                .iter()
                .any(|value| value.contains("read-only")
                    || value.starts_with("default_permissions="))
        );
    }
}
