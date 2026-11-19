//! The checks on a commit, until they decide.
//!
//! Failed checks are reported as they fail, even while others still run; the verdict ends the
//! watch with the end of each failed job's log. A workflow that armed the watch also gets its CI
//! prompt appended, so the agent is told what to do about the verdict, not just what it was.

use std::time::Duration;
use std::time::Instant;

use nori_forge::CheckState;
use nori_forge::Checks;
use nori_forge::RunState;
use tokio::sync::mpsc::Sender;

use super::Item;
use crate::backend::forge_tools;
use crate::backend::monitor::event::Ending;
use crate::backend::monitor::spec::CiFollowUp;

/// The waits between reads of a commit's checks; the last repeats.
const POLLS: [Duration; 4] = [
    Duration::from_secs(10),
    Duration::from_secs(15),
    Duration::from_secs(30),
    Duration::from_secs(60),
];
/// A commit with no checks this long after the watch began has no CI, or it has not started.
const NO_CI: Duration = Duration::from_secs(3 * 60);
/// Reads in a row that may fail before the watch stops.
const MAX_ERRORS: i64 = 5;
const LOG_LINES: i64 = 30;
const LOGS_READ: usize = 3;

/// The commit being watched, and where its forge is.
pub(in crate::backend::monitor) struct CiTarget {
    place: forge_tools::Place,
    sha: String,
}

impl CiTarget {
    /// `sha`, or the commit `branch` (default: the checked-out one) was last pushed at.
    pub(super) async fn resolve(
        cwd: &std::path::Path,
        sha: Option<&str>,
        branch: Option<&str>,
    ) -> Result<Self, String> {
        let place = forge_tools::place(cwd.to_path_buf(), branch.map(str::to_owned)).await?;
        let sha = match sha {
            Some(sha) => sha.to_owned(),
            None => place
                .forge
                .branch_head(&place.branch)
                .await
                .map_err(|error| error.to_string())?
                .ok_or_else(|| {
                    format!(
                        "`{}` was never pushed, so there are no checks to watch. Push it first.",
                        place.branch
                    )
                })?,
        };
        Ok(Self { place, sha })
    }
}

pub(super) async fn run(
    target: CiTarget,
    follow_up: Option<CiFollowUp>,
    tx: &Sender<Item>,
) -> Option<Ending> {
    let CiTarget { place, sha } = target;
    let short = sha.get(..7).unwrap_or(&sha).to_owned();
    let started = Instant::now();
    let mut waits = POLLS.into_iter().chain(std::iter::repeat(POLLS[3]));
    let mut errors = 0;
    let mut reported: Vec<String> = Vec::new();
    loop {
        tokio::time::sleep(waits.next().unwrap_or(POLLS[3])).await;
        let checks = match place.forge.checks(&sha).await {
            Ok(checks) => checks,
            Err(error) => {
                errors += 1;
                if errors >= MAX_ERRORS {
                    return Some(Ending::Failed {
                        reason: format!("the checks on {short} could not be read: {error}"),
                    });
                }
                continue;
            }
        };
        errors = 0;
        for run in checks.in_states(&[RunState::Failed]) {
            if !reported.contains(&run.name) {
                reported.push(run.name.clone());
                let url = run
                    .url
                    .as_deref()
                    .map(|url| format!(" ({url})"))
                    .unwrap_or_default();
                let line = Item::Line {
                    text: format!("check `{}` failed on {short}{url}", run.name),
                    failure: true,
                };
                if tx.send(line).await.is_err() {
                    return None;
                }
            }
        }
        let failed = match checks.state() {
            // A failure decides, even while other checks run.
            CheckState::Failing => true,
            CheckState::Passing => false,
            CheckState::Cancelled => {
                return Some(Ending::Finished {
                    ok: false,
                    note: format!("The checks on {short} were cancelled."),
                });
            }
            CheckState::None if started.elapsed() > NO_CI => {
                return Some(match follow_up {
                    Some(_) => Ending::Quiet,
                    None => Ending::Finished {
                        ok: false,
                        note: format!(
                            "No checks appeared on {short} within {} minutes: it has no CI, or CI \
                             has not started. Nothing observed says the checks passed.",
                            NO_CI.as_secs() / 60
                        ),
                    },
                });
            }
            CheckState::None | CheckState::Pending => continue,
        };
        let (summary, mut lines) = report(&place, &checks, failed).await;
        if let Some(follow_up) = &follow_up {
            let prompt = if failed {
                follow_up.on_failed.as_deref()
            } else {
                follow_up.on_passed.as_deref()
            };
            if let Some(prompt) = prompt {
                lines.push(format!("Workflow \"{}\": {prompt}", follow_up.workflow));
            }
        }
        for text in lines {
            if tx
                .send(Item::Line {
                    text,
                    failure: false,
                })
                .await
                .is_err()
            {
                return None;
            }
        }
        return Some(Ending::Finished {
            ok: !failed,
            note: summary,
        });
    }
}

/// The verdict on `checks`: one line for the event's end, and the lines of the report: each
/// failed check with the end of its log, or how many passed.
async fn report(
    place: &forge_tools::Place,
    checks: &Checks,
    failed: bool,
) -> (String, Vec<String>) {
    let short = checks.sha.get(..7).unwrap_or(&checks.sha);
    let failures: Vec<_> = checks.in_states(&[RunState::Failed]).collect();
    if !failed || failures.is_empty() {
        let passed = checks.in_states(&[RunState::Passed]).count();
        let summary = format!(
            "CI passed on {short} (`{}`): {passed} checks passed.",
            place.branch
        );
        return (summary, Vec::new());
    }
    let names: Vec<&str> = failures.iter().map(|run| run.name.as_str()).collect();
    let summary = format!("CI failed on {short}: {}.", names.join(", "));
    let logs = failures
        .iter()
        .filter_map(|run| run.job)
        .take(LOGS_READ)
        .map(|job| async move { (job, place.forge.job_log(job).await.ok()) });
    let logs: std::collections::HashMap<i64, Option<String>> =
        futures::future::join_all(logs).await.into_iter().collect();
    let mut lines = Vec::new();
    for run in &failures {
        let url = run
            .url
            .as_deref()
            .map(|url| format!(" ({url})"))
            .unwrap_or_default();
        lines.push(format!("- {}{url}", run.name));
        if let Some(Some(log)) = run.job.and_then(|job| logs.get(&job)) {
            lines.extend(
                nori_forge::log_excerpt(log, LOG_LINES)
                    .lines()
                    .map(|line| format!("    {line}")),
            );
        }
    }
    (summary, lines)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[tokio::test]
    async fn a_failure_report_names_each_failed_check_with_its_log() {
        use wiremock::Mock;
        use wiremock::MockServer;
        use wiremock::ResponseTemplate;
        use wiremock::matchers::path;

        let server = MockServer::start().await;
        Mock::given(path("/repos/o/r/actions/jobs/7/logs"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "2026-11-16T10:00:00.0000001Z error[E0308]: mismatched types\n2026-11-16T10:00:01.0000001Z ##[error]Process completed with exit code 101.\n",
            ))
            .mount(&server)
            .await;
        let repo = nori_forge::ForgeRepo {
            host: "github.com".to_owned(),
            owner: "o".to_owned(),
            name: "r".to_owned(),
        };
        let place = forge_tools::Place {
            forge: std::sync::Arc::new(nori_forge::Forge::new(
                repo,
                server.uri(),
                Some("t".to_owned()),
            )),
            branch: "topic".to_owned(),
            head: None,
            remote: "origin".to_owned(),
            default_branch: None,
            cwd: std::path::PathBuf::from("/tmp"),
        };
        let run = |name: &str, state, job| nori_forge::CheckRun {
            name: name.to_owned(),
            state,
            url: Some(format!("https://ci/{name}")),
            job,
        };
        let checks = Checks {
            sha: "0123456789".to_owned(),
            runs: vec![
                run("test", RunState::Failed, Some(7)),
                run("lint", RunState::Failed, None),
                run("build", RunState::Passed, Some(8)),
            ],
        };
        let (summary, lines) = report(&place, &checks, true).await;
        assert_eq!(summary, "CI failed on 0123456: test, lint.");
        assert_eq!(
            lines,
            vec![
                "- test (https://ci/test)",
                "    error[E0308]: mismatched types",
                "    error: Process completed with exit code 101.",
                "- lint (https://ci/lint)",
            ]
        );
    }
}
