//! Pkl workflow policy loaded in default, global, then project order.
use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;

use crate::EvaluatorOptions;
use nori_workflow::CommitAgent;
use nori_workflow::CommitBatch;
use nori_workflow::CommitRules;
use nori_workflow::CommitStyle;
use nori_workflow::CompactionOverride;
use nori_workflow::FallbackOverride;
use nori_workflow::GoalsOverride;
use nori_workflow::Hook;
use nori_workflow::HookEvent;
use nori_workflow::LoopAction;
use nori_workflow::LoopOverride;
use nori_workflow::PrRules;
use nori_workflow::RecoveryLayer;
use nori_workflow::RecoveryOverride;
use nori_workflow::Route;
use nori_workflow::Upstream;
use nori_workflow::Workflow;
use serde::Deserialize;
use serde::Deserializer;

use crate::Error;
use crate::recovery::DurationValue;
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
    compaction: Option<PolicyCompaction>,
    loops: Option<PolicyLoops>,
    goals: Option<PolicyGoals>,
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

/// The `compaction` block: every field optional, so a layer states only what it changes.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PolicyCompaction {
    auto: Option<bool>,
    trigger_free_fraction: Option<f64>,
    trigger_free_tokens: Option<i64>,
    background_margin_tokens: Option<i64>,
    shrink_ladder: Option<Vec<f64>>,
    prefer_native: Option<bool>,
}

/// The `loops` block: every field optional, so a layer states only what it changes.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PolicyLoops {
    window: Option<i64>,
    repeats: Option<i64>,
    action: Option<String>,
}

/// The `goals` block: every field optional, so a layer states only what it changes. The agent and
/// model are nullable, so a layer can set them back to the session's own and the agent's default.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PolicyGoals {
    judge: Option<bool>,
    #[serde(deserialize_with = "deserialize_present_nullable")]
    judge_agent: Option<Option<String>>,
    #[serde(deserialize_with = "deserialize_present_nullable")]
    judge_model: Option<Option<String>>,
    judge_timeout: Option<DurationValue>,
    max_disagreements: Option<i64>,
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
        if let Some(compaction) = self.compaction {
            CompactionOverride {
                auto: compaction.auto,
                trigger_free_fraction: compaction.trigger_free_fraction,
                trigger_free_tokens: compaction.trigger_free_tokens,
                background_margin_tokens: compaction.background_margin_tokens,
                shrink_ladder: compaction.shrink_ladder,
                prefer_native: compaction.prefer_native,
            }
            .apply(&mut workflow.compaction);
            workflow
                .compaction
                .validate()
                .map_err(|message| Error::Invalid(format!("compaction: {message}")))?;
        }
        if let Some(loops) = self.loops {
            let action = loops
                .action
                .map(|name| {
                    LoopAction::parse(&name).ok_or_else(|| {
                        Error::Invalid(format!(
                            "loops: unknown action {name:?}, expected nudge_then_stop, nudge, stop or off"
                        ))
                    })
                })
                .transpose()?;
            LoopOverride {
                window: loops.window,
                repeats: loops.repeats,
                action,
            }
            .apply(&mut workflow.loops);
            workflow
                .loops
                .validate()
                .map_err(|message| Error::Invalid(format!("loops: {message}")))?;
        }
        if let Some(goals) = self.goals {
            GoalsOverride {
                judge: goals.judge,
                judge_agent: goals.judge_agent,
                judge_model: goals.judge_model,
                judge_timeout: goals
                    .judge_timeout
                    .as_ref()
                    .map(DurationValue::to_duration)
                    .transpose()
                    .map_err(|message| Error::Invalid(format!("goals.judge_timeout: {message}")))?,
                max_disagreements: goals.max_disagreements,
            }
            .apply(&mut workflow.goals);
            workflow
                .goals
                .validate()
                .map_err(|message| Error::Invalid(format!("goals: {message}")))?;
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
    fn compaction_layers_field_by_field_over_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("init.pkl");
        let project = dir.path().join("project.pkl");
        std::fs::write(
            &global,
            r#"compaction = new Dynamic {
          trigger_free_fraction = 0.25
          shrink_ladder = List(0.9, 0.4)
          prefer_native = false
        }"#,
        )
        .unwrap();
        std::fs::write(
            &project,
            r#"compaction = new Dynamic { auto = false; trigger_free_tokens = 4000 }"#,
        )
        .unwrap();
        let policy = load_policy(Some(&global), Some(&project)).unwrap();
        assert_eq!(
            policy.compaction,
            nori_workflow::CompactionPolicy {
                auto: false,
                trigger_free_fraction: 0.25,
                trigger_free_tokens: 4000,
                background_margin_tokens: 12_000,
                shrink_ladder: vec![0.9, 0.4],
                prefer_native: false,
            }
        );
        assert_eq!(policy.loops, nori_workflow::LoopPolicy::default());
    }

    #[test]
    fn loops_layer_field_by_field_and_validate_the_merged_result() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("init.pkl");
        let project = dir.path().join("project.pkl");
        std::fs::write(
            &global,
            r#"loops = new Dynamic { window = 20; action = "nudge" }"#,
        )
        .unwrap();
        std::fs::write(&project, r#"loops = new Dynamic { repeats = 8 }"#).unwrap();
        let policy = load_policy(Some(&global), Some(&project)).unwrap();
        assert_eq!(
            policy.loops,
            nori_workflow::LoopPolicy {
                window: 20,
                repeats: 8,
                action: LoopAction::Nudge,
            }
        );

        // Each layer is fine alone, but the merged result (window 4, repeats 5 from the
        // defaults... here 5 is still the built-in value) could never find a loop.
        std::fs::write(&global, "name = \"plain\"").unwrap();
        std::fs::write(&project, r#"loops = new Dynamic { window = 4 }"#).unwrap();
        let error = load_policy(Some(&global), Some(&project)).unwrap_err();
        assert!(error.to_string().contains("repeats"), "{error}");
    }

    #[test]
    fn goals_layer_field_by_field_and_null_resets_the_agent() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("init.pkl");
        let project = dir.path().join("project.pkl");
        std::fs::write(
            &global,
            r#"goals = new Dynamic {
          judge_agent = "codex"
          judge_model = "mini"
          judge_timeout = 2.min
          max_disagreements = 5
        }"#,
        )
        .unwrap();
        std::fs::write(
            &project,
            r#"goals = new Dynamic { judge_model = null; judge_timeout = 30 }"#,
        )
        .unwrap();
        let policy = load_policy(Some(&global), Some(&project)).unwrap();
        assert_eq!(
            policy.goals,
            nori_workflow::GoalsPolicy {
                judge: true,
                judge_agent: Some("codex".into()),
                judge_model: None,
                judge_timeout: std::time::Duration::from_secs(30),
                max_disagreements: 5,
            }
        );

        std::fs::write(&project, r#"goals = new Dynamic { judge = false }"#).unwrap();
        let policy = load_policy(Some(&global), Some(&project)).unwrap();
        assert_eq!(
            policy.goals,
            nori_workflow::GoalsPolicy {
                judge: false,
                judge_agent: Some("codex".into()),
                judge_model: Some("mini".into()),
                judge_timeout: std::time::Duration::from_secs(120),
                max_disagreements: 5,
            }
        );
    }

    #[test]
    fn bad_goal_values_are_policy_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("init.pkl");
        for (source, expected) in [
            (
                r#"goals = new Dynamic { max_disagreements = 0 }"#,
                "max_disagreements",
            ),
            (
                r#"goals = new Dynamic { judge_timeout = 0.s }"#,
                "judge_timeout",
            ),
            (
                r#"goals = new Dynamic { judge_timeout = 2.h }"#,
                "judge_timeout",
            ),
            (
                r#"goals = new Dynamic { judge_agent = " " }"#,
                "judge_agent",
            ),
            (r#"goals = new Dynamic { mystery = 1 }"#, "mystery"),
        ] {
            std::fs::write(&path, source).unwrap();
            let error = load_policy(Some(&path), None).unwrap_err().to_string();
            assert!(error.contains(expected), "{source}: {error}");
        }
    }

    #[test]
    fn bad_compaction_and_loop_values_are_policy_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("init.pkl");
        for (source, expected) in [
            (
                r#"compaction = new Dynamic { trigger_free_fraction = 2.0 }"#,
                "trigger_free_fraction",
            ),
            (
                r#"compaction = new Dynamic { shrink_ladder = List(0.5, 0.6) }"#,
                "shrink_ladder",
            ),
            (r#"compaction = new Dynamic { mystery = 1 }"#, "mystery"),
            (
                r#"loops = new Dynamic { action = "panic" }"#,
                "unknown action",
            ),
            (
                r#"loops = new Dynamic { window = 3; repeats = 3 }"#,
                "repeats",
            ),
            (r#"loops = new Dynamic { window = 0 }"#, "window"),
        ] {
            std::fs::write(&path, source).unwrap();
            let error = load_policy(Some(&path), None).unwrap_err().to_string();
            assert!(error.contains(expected), "{source}: {error}");
        }
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
        evaluated.compaction = nori_workflow::CompactionPolicy {
            auto: false,
            trigger_free_fraction: 0.5,
            trigger_free_tokens: 1,
            background_margin_tokens: 1,
            shrink_ladder: Vec::new(),
            prefer_native: false,
        };
        evaluated.loops = nori_workflow::LoopPolicy {
            window: 2,
            repeats: 1,
            action: LoopAction::Off,
        };
        evaluated.goals = nori_workflow::GoalsPolicy {
            judge: false,
            judge_agent: Some("stale".into()),
            judge_model: Some("stale".into()),
            judge_timeout: std::time::Duration::from_secs(1),
            max_disagreements: 99,
        };
        policy.apply(&mut evaluated).unwrap();
        assert_eq!(builtin_policy(), &evaluated);
        let other = std::thread::spawn(builtin_policy).join().unwrap();
        assert!(std::ptr::eq(builtin_policy(), other));
    }
}
