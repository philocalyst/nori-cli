//! Pkl workflow policy loaded in default, global, then project order.
use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;

use crate::EvaluatorOptions;
use nori_workflow::CommitAgent;
use nori_workflow::CommitBatch;
use nori_workflow::CommitRules;
use nori_workflow::CommitStyle;
use nori_workflow::FallbackOverride;
use nori_workflow::Hook;
use nori_workflow::HookEvent;
use nori_workflow::PrRules;
use nori_workflow::RecoveryLayer;
use nori_workflow::RecoveryOverride;
use nori_workflow::Route;
use nori_workflow::Upstream;
use nori_workflow::Workflow;
use serde::Deserialize;
use serde::Deserializer;

use crate::Error;
use crate::recovery::RecoveryLayerBlock;

pub fn global_policy_path(home: &Path) -> PathBuf {
    home.join("init.pkl")
}
pub fn project_policy_path(cwd: &Path) -> PathBuf {
    cwd.join(".nori").join("nori.pkl")
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Policy {
    name: Option<String>,
    max_turns: Option<i64>,
    /// Legacy spellings of `recovery.fallback.ranked` and `recovery.fallback.chain`.
    fallback_ranked: Option<bool>,
    fallback_models: Option<Vec<String>>,
    recovery: Option<RecoveryLayerBlock>,
    instructions: Option<Vec<String>>,
    hooks: Option<Vec<PolicyHook>>,
    commit: Option<PolicyCommit>,
    #[serde(default, deserialize_with = "deserialize_present_nullable")]
    commit_reviewer: Option<Option<PolicyAgent>>,
    pr: Option<PolicyPr>,
    tools: Option<PolicyTools>,
}

fn deserialize_present_nullable<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PolicyTools {
    enabled: Option<Vec<String>>,
    disabled: Option<Vec<String>>,
    descriptions: Option<std::collections::BTreeMap<String, String>>,
    instructions: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyHook {
    event: String,
    prompt: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyAgent {
    agent: String,
    model: Option<String>,
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PolicyCommit {
    style: Option<String>,
    max_files: Option<i64>,
    max_subject: Option<i64>,
    atomic: bool,
    batch: Option<String>,
    require_body: bool,
    agent: Option<PolicyAgent>,
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PolicyPr {
    title: Option<String>,
    sections: Vec<String>,
    max_lines: Option<i64>,
    max_files: Option<i64>,
    base: Option<String>,
    draft: bool,
    upstream: Option<String>,
}

/// Load defaults followed by the existing user and trusted project policies.
/// User-owned policies can use Pkl imports and resources. They are never used
/// for journal replay, which reads recorded data only.
pub fn load_policy(global: Option<&Path>, project: Option<&Path>) -> Result<Workflow, Error> {
    load_policy_with_machine(global, project, &crate::machine::MachineSnapshot::capture())
}

/// Evaluate using a captured, replayable machine snapshot.
pub fn load_policy_with_machine(
    global: Option<&Path>,
    project: Option<&Path>,
    snapshot: &crate::machine::MachineSnapshot,
) -> Result<Workflow, Error> {
    let mut workflow = builtin_policy().clone();
    let mut paths = Vec::new();
    for path in [global, project].into_iter().flatten() {
        match std::fs::symlink_metadata(path) {
            Ok(_) => paths.push(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(Error::Evaluation(format!(
                    "cannot inspect policy {}: {error}",
                    path.display()
                )));
            }
        }
    }
    if paths.is_empty() {
        return Ok(workflow);
    }
    let options = EvaluatorOptions::preconfigured().properties(snapshot.properties());
    for path in paths {
        let policy: Policy =
            crate::native::evaluate_path(path, &options).map_err(Error::Evaluation)?;
        policy.apply(&mut workflow)?;
    }
    Ok(workflow)
}

/// Compiled value of the input-free `default.pkl` module. Custom user and
/// project modules run through the native Rust evaluator. The test below
/// compares this value to that module so changes cannot silently drift.
fn builtin_policy() -> &'static Workflow {
    static BUILTIN: OnceLock<Workflow> = OnceLock::new();
    BUILTIN.get_or_init(|| Workflow {
        name: "nori-default".to_owned(),
        ..Workflow::default()
    })
}

impl Policy {
    fn apply(self, workflow: &mut Workflow) -> Result<(), Error> {
        if let Some(name) = self.name {
            workflow.name = name;
        }
        if let Some(turns) = self.max_turns {
            if turns < 0 {
                return Err(Error::Invalid("max_turns must be nonnegative".into()));
            }
            workflow.max_turns = Some(turns);
        }
        // The legacy fallback spellings apply first, so a `recovery` block in the same
        // module wins over them.
        if self.fallback_models.is_some() || self.fallback_ranked.is_some() {
            let chain = self.fallback_models.as_ref().map(|models| {
                models
                    .iter()
                    .map(|id| Route::parse_legacy(id))
                    .collect::<Vec<_>>()
            });
            // A non-empty explicit list used to turn the ranked catalog off unless asked for.
            let ranked = self.fallback_ranked.or(self
                .fallback_models
                .as_ref()
                .filter(|models| !models.is_empty())
                .map(|_| false));
            workflow.recovery.apply(RecoveryLayer {
                base: RecoveryOverride {
                    fallback: FallbackOverride {
                        chain,
                        ranked,
                        ..FallbackOverride::default()
                    },
                    ..RecoveryOverride::default()
                },
                ..RecoveryLayer::default()
            });
        }
        if let Some(recovery) = self.recovery {
            let layer = recovery
                .to_layer()
                .map_err(|message| Error::Invalid(format!("recovery: {message}")))?;
            workflow.recovery.apply(layer);
        }
        if let Some(tools) = self.tools {
            let selection = workflow.tools.get_or_insert_with(Default::default);
            if let Some(enabled) = tools.enabled {
                selection.enabled = Some(enabled);
            }
            if let Some(disabled) = tools.disabled {
                selection.disabled = disabled;
            }
            if let Some(descriptions) = tools.descriptions {
                selection.descriptions = descriptions;
            }
            if let Some(instructions) = tools.instructions {
                selection.instructions = instructions;
            }
        }
        if let Some(instructions) = self.instructions {
            workflow.instructions = instructions;
        }
        if let Some(hooks) = self.hooks {
            workflow.hooks = hooks
                .into_iter()
                .map(|hook| {
                    Ok(Hook {
                        event: match hook.event.as_str() {
                            "on-change" => HookEvent::AfterChange,
                            "on-turn" => HookEvent::AfterTurn,
                            "on-commit" => HookEvent::AfterCommit,
                            "on-pr" => HookEvent::BeforePr,
                            "on-stop" => HookEvent::BeforeStop,
                            "on-ci-fail" => HookEvent::CiFailed,
                            "on-ci-pass" => HookEvent::CiPassed,
                            other => {
                                return Err(Error::Invalid(format!(
                                    "unknown hook event {other:?}"
                                )));
                            }
                        },
                        prompt: hook.prompt,
                    })
                })
                .collect::<Result<Vec<_>, Error>>()?;
        }
        if let Some(reviewer) = self.commit_reviewer {
            workflow.commit_reviewer = reviewer.map(|agent| CommitAgent {
                agent: agent.agent,
                model: agent.model,
            });
        }
        if let Some(c) = self.commit {
            workflow.commit = Some(CommitRules {
                style: c
                    .style
                    .map(|s| match s.as_str() {
                        "plain" => Ok(CommitStyle::Plain),
                        "conventional" => Ok(CommitStyle::Conventional),
                        _ => Err(Error::Invalid(format!("unknown commit style {s:?}"))),
                    })
                    .transpose()?,
                batch: c
                    .batch
                    .map(|s| match s.as_str() {
                        "per-change" => Ok(CommitBatch::PerChange),
                        "per-turn" => Ok(CommitBatch::PerTurn),
                        "per-task" => Ok(CommitBatch::PerTask),
                        _ => Err(Error::Invalid(format!("unknown commit batch {s:?}"))),
                    })
                    .transpose()?,
                max_files: c.max_files,
                max_subject: c.max_subject,
                atomic: c.atomic,
                require_body: c.require_body,
                agent: c.agent.map(|a| CommitAgent {
                    agent: a.agent,
                    model: a.model,
                }),
            });
        }
        if let Some(pr) = self.pr {
            workflow.pr = Some(PrRules {
                title: pr.title,
                sections: pr.sections,
                max_lines: pr.max_lines,
                max_files: pr.max_files,
                base: pr.base,
                draft: pr.draft,
                upstream: pr
                    .upstream
                    .map(|s| match s.as_str() {
                        "never" => Ok(Upstream::Never),
                        "ask" => Ok(Upstream::Ask),
                        "tests-pass" => Ok(Upstream::TestsPass),
                        "done" => Ok(Upstream::Done),
                        _ => Err(Error::Invalid(format!("unknown upstream rule {s:?}"))),
                    })
                    .transpose()?,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn loads_default_global_and_project_policy() {
        let temp = tempfile::tempdir().unwrap();
        let global = temp.path().join("init.pkl");
        let project = temp.path().join("nori.pkl");
        std::fs::write(
            &global,
            r#"
          fallback_ranked = false
          fallback_models = List("one", "two")
          commit_reviewer = new Dynamic { agent = "claude"; model = "haiku" }
        "#,
        )
        .unwrap();
        std::fs::write(
            &project,
            r#"
          fallback_models = List("project")
          instructions = List("Check the tests")
        "#,
        )
        .unwrap();
        let policy = load_policy(Some(&global), Some(&project)).unwrap();
        assert_eq!(policy.name, "nori-default");
        assert_eq!(policy.max_turns, None);
        assert!(!policy.recovery.base.fallback.ranked);
        assert_eq!(
            policy.recovery.base.fallback.chain,
            vec![nori_workflow::Route::parse_legacy("project")]
        );
        assert_eq!(
            policy.commit_reviewer,
            Some(CommitAgent {
                agent: "claude".into(),
                model: Some("haiku".into())
            })
        );
        assert_eq!(policy.instructions, vec!["Check the tests"]);
    }

    #[test]
    fn tool_selection_layers_by_field() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("init.pkl");
        let project = dir.path().join("project.pkl");
        std::fs::write(
            &global,
            r#"tools = new Dynamic {
          enabled = List("get_goal", "create_goal")
          descriptions = new Mapping { ["get_goal"] = "Read the goal" }
          instructions = List("Use the goal carefully")
        }"#,
        )
        .unwrap();
        std::fs::write(
            &project,
            r#"tools = new Dynamic { disabled = List("create_goal") }"#,
        )
        .unwrap();
        let policy = load_policy(Some(&global), Some(&project)).unwrap();
        assert_eq!(
            policy.tools,
            Some(nori_workflow::ToolPolicy {
                enabled: Some(vec!["get_goal".into(), "create_goal".into()]),
                disabled: vec!["create_goal".into()],
                descriptions: [("get_goal".into(), "Read the goal".into())]
                    .into_iter()
                    .collect(),
                instructions: vec!["Use the goal carefully".into()],
            })
        );
    }

    #[test]
    fn project_null_clears_global_commit_reviewer_but_omission_keeps_it() {
        let temp = tempfile::tempdir().unwrap();
        let global = temp.path().join("init.pkl");
        let project = temp.path().join("project.pkl");
        std::fs::write(
            &global,
            r#"commit_reviewer = new Dynamic { agent = "claude"; model = "haiku" }"#,
        )
        .unwrap();

        std::fs::write(&project, "name = \"project\"").unwrap();
        let omitted = load_policy(Some(&global), Some(&project)).unwrap();
        assert_eq!(
            omitted.commit_reviewer,
            Some(CommitAgent {
                agent: "claude".into(),
                model: Some("haiku".into())
            })
        );

        std::fs::write(&project, "commit_reviewer = null").unwrap();
        let cleared = load_policy(Some(&global), Some(&project)).unwrap();
        assert_eq!(cleared.commit_reviewer, None);
    }
    #[test]
    fn supplied_machine_snapshot_is_available_to_pkl() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("init.pkl");
        std::fs::write(
            &path,
            r#"instructions = List(read("prop:machine.cpu.available_parallelism"))"#,
        )
        .unwrap();
        let snapshot = crate::machine::MachineSnapshot {
            cpu_available_parallelism: Some(7),
            cpu_load_average: None,
            memory_total_bytes: None,
            memory_available_bytes: None,
        };
        let policy = load_policy_with_machine(Some(&path), None, &snapshot).unwrap();
        assert_eq!(policy.instructions, vec!["7"]);
    }
    #[test]
    fn missing_custom_files_use_compiled_default() {
        let dir = tempfile::tempdir().unwrap();
        let absent = dir.path().join("missing.pkl");
        assert_eq!(
            load_policy(Some(&absent), None).unwrap(),
            builtin_policy().clone()
        );
    }

    #[cfg(unix)]
    #[test]
    fn inaccessible_policy_path_is_not_silently_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cycle.pkl");
        std::os::unix::fs::symlink("cycle.pkl", &path).unwrap();
        let error = load_policy(Some(&path), None).unwrap_err();
        assert!(!error.to_string().is_empty());
    }

    #[test]
    fn compiled_builtin_matches_the_native_pkl_module() {
        let policy: Policy = crate::native::evaluate(
            include_str!("../default.pkl"),
            &EvaluatorOptions::preconfigured(),
        )
        .unwrap();
        // Start from values the module must overwrite, so a field it forgets to state fails.
        let mut evaluated = Workflow::default();
        evaluated.recovery.base.retry.max_attempts = -1;
        evaluated.recovery.base.fallback.ranked = false;
        evaluated.recovery.base.fallback.prefer_headroom = false;
        policy.apply(&mut evaluated).unwrap();
        assert_eq!(builtin_policy(), &evaluated);
        let other = std::thread::spawn(builtin_policy).join().unwrap();
        assert!(std::ptr::eq(builtin_policy(), other));
    }
}
