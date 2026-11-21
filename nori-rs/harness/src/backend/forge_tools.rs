//! The forge tools: how the branch stands on GitHub (`ci_status`), and opening or updating its
//! pull request by the workflow's rules (`pull_request`). They call GitHub's API directly
//! through `nori_forge`; nothing shells out to `gh`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;

use nori_forge::Forge;
use nori_forge::NewPull;
use nori_forge::RunState;
use nori_workflow::Upstream;
use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::schemars;
use rmcp::tool;
use rmcp::tool_router;
use serde::Deserialize;
use serde_json::json;

use super::nori_client_mcp::NoriClientService;
use super::nori_client_mcp::Workspace;
use super::nori_client_mcp::tool_error;
use super::nori_client_mcp::tool_success;

/// What agents are told about the forge tools, after the git tools.
pub(crate) const INSTRUCTIONS: &str = "Pull requests and CI: push the branch yourself (`git push -u`), then open or update its \
     pull request with `pull_request`, which applies the workflow's pull request rules. \
     `ci_status` reports the branch's pull request and checks; pass `logs` to see why a check \
     failed.";

/// Lines of each failed job's log that `ci_status` returns.
const LOG_LINES: i64 = 40;
/// Failed jobs whose logs one `ci_status` reads.
const LOGS_READ: usize = 3;

/// One client per remote, so conditional reads remember their answers across calls and sessions.
static FORGES: LazyLock<std::sync::Mutex<HashMap<String, Arc<Forge>>>> =
    LazyLock::new(Default::default);

#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub(crate) struct CiStatusRequest {
    /// The branch to look at. Default: the checked-out branch.
    #[serde(default)]
    pub(crate) branch: Option<String>,
    /// Also return the end of each failed GitHub Actions job's log. Default: false.
    #[serde(default)]
    pub(crate) logs: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub(crate) struct PullRequestRequest {
    /// Required. The pull request's title.
    pub(crate) title: String,
    /// Required. Its description, in Markdown.
    pub(crate) body: String,
    /// The branch to merge into. Default: the workflow's base, else the remote's default branch.
    #[serde(default)]
    pub(crate) base: Option<String>,
    /// Open it as a draft. Default: as the workflow says, else not.
    #[serde(default)]
    pub(crate) draft: Option<bool>,
}

#[tool_router(router = forge_tool_router, vis = "pub(crate)")]
impl NoriClientService {
    #[tool(
        description = "How this branch (or `branch`) stands on GitHub: its pull request, whether HEAD is pushed, and the checks on the pushed commit, failed and unfinished ones by name. With `logs`, the end of each failed Actions job's log. A snapshot: to wait for checks, arm `monitor_start` with `source: ci` instead of calling this again.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    pub(crate) async fn ci_status(
        &self,
        Parameters(CiStatusRequest { branch, logs }): Parameters<CiStatusRequest>,
    ) -> Result<CallToolResult, McpError> {
        let Some(cwd) = self.shared_cwd() else {
            return Ok(tool_error("this session has no working directory"));
        };
        let status = async {
            let place = place(cwd, branch).await?;
            status(&place, logs.unwrap_or(false)).await
        };
        Ok(match status.await {
            Ok(body) => tool_success(body),
            Err(message) => tool_error(message),
        })
    }

    #[tool(
        description = "Open a pull request from the pushed current branch, or update the open one's title and description. Push the branch first. Checks the workflow's pull request rules and may ask the user first. Returns the pull request.",
        annotations(
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    pub(crate) async fn pull_request(
        &self,
        Parameters(request): Parameters<PullRequestRequest>,
    ) -> Result<CallToolResult, McpError> {
        let Some(workspace) = self.workspace().cloned() else {
            return Ok(tool_error("this session has no working directory"));
        };
        Ok(
            match pull_request(workspace, request, self.orchestrator()).await {
                Ok(body) => tool_success(body),
                Err(message) => tool_error(message),
            },
        )
    }
}

/// A branch, the repository it is in, and the forge that repository lives on.
pub(crate) struct Place {
    pub(crate) forge: Arc<Forge>,
    pub(crate) branch: String,
    /// The branch's commit here, when it is the checked-out branch.
    pub(crate) head: Option<String>,
    pub(crate) remote: String,
    /// The remote's default branch, as last fetched.
    pub(crate) default_branch: Option<String>,
    pub(crate) cwd: PathBuf,
}

/// `branch` (default: the checked-out one) of the repository at `cwd`, on its forge.
pub(crate) async fn place(cwd: PathBuf, branch: Option<String>) -> Result<Place, String> {
    let dir = cwd.clone();
    let (url, branch, head, remote, default_branch) = tokio::task::spawn_blocking(move || {
        let repo = nori_config::GitRepo::discover(&dir)
            .ok_or("the working directory is not inside a git repository")?;
        let current = repo.branch();
        let branch = branch
            .or_else(|| current.clone())
            .ok_or("HEAD is not on a branch")?;
        let head = (current.as_deref() == Some(branch.as_str()))
            .then(|| repo.head_commit())
            .flatten();
        let remote = repo.push_remote();
        let url = repo
            .remote_url(&remote)
            .ok_or_else(|| format!("this repository has no remote `{remote}`"))?;
        let default_branch = repo.remote_default_branch(&remote);
        Ok::<_, String>((url, branch, head, remote, default_branch))
    })
    .await
    .map_err(|error| error.to_string())??;
    let forge = {
        let mut forges = FORGES.lock().map_err(|error| error.to_string())?;
        match forges.get(&url) {
            Some(forge) => Arc::clone(forge),
            None => {
                let forge = Arc::new(Forge::for_remote(&url).map_err(|error| error.to_string())?);
                forges.insert(url, Arc::clone(&forge));
                forge
            }
        }
    };
    Ok(Place {
        forge,
        branch,
        head,
        remote,
        default_branch,
        cwd,
    })
}

/// What `ci_status` answers: the pull request, the pushed commit, and its checks in brief.
async fn status(place: &Place, logs: bool) -> Result<serde_json::Value, String> {
    let forge = &place.forge;
    let (pull, pushed) = tokio::join!(
        forge.pull_for_branch(&place.branch),
        forge.branch_head(&place.branch)
    );
    let pull = pull.map_err(|error| error.to_string())?;
    let pushed = pushed.map_err(|error| error.to_string())?;
    let mut body = json!({
        "repo": forge.repo().slug(),
        "branch": place.branch,
        "pushed": pushed,
        "pull_request": pull,
    });
    if let Some(head) = &place.head {
        body["head_pushed"] = json!(pushed.as_ref() == Some(head));
    }
    let Some(sha) = pushed else {
        body["checks"] = serde_json::Value::Null;
        return Ok(body);
    };
    let checks = forge
        .checks(&sha)
        .await
        .map_err(|error| error.to_string())?;
    let mut summary = json!({
        "state": checks.state(),
        "passed": checks.in_states(&[RunState::Passed]).count(),
    });
    for (key, states) in [
        ("running", &[RunState::Running, RunState::Pending][..]),
        ("cancelled", &[RunState::Cancelled][..]),
    ] {
        let runs: Vec<_> = checks.in_states(states).collect();
        if !runs.is_empty() {
            summary[key] = json!(runs);
        }
    }
    let failed: Vec<_> = checks.in_states(&[RunState::Failed]).collect();
    if !failed.is_empty() {
        let jobs: Vec<i64> = if logs {
            failed
                .iter()
                .filter_map(|run| run.job)
                .take(LOGS_READ)
                .collect()
        } else {
            Vec::new()
        };
        let read = jobs
            .into_iter()
            .map(|job| async move { (job, forge.job_log(job).await) });
        let read: HashMap<i64, Result<String, nori_forge::ForgeError>> =
            futures::future::join_all(read).await.into_iter().collect();
        summary["failed"] = failed
            .iter()
            .map(|run| {
                let mut entry = json!(run);
                match run.job.and_then(|job| read.get(&job)) {
                    Some(Ok(log)) => entry["log"] = json!(nori_forge::log_excerpt(log, LOG_LINES)),
                    Some(Err(error)) => entry["log_error"] = json!(error.to_string()),
                    None => {}
                }
                entry
            })
            .collect();
    }
    body["checks"] = summary;
    Ok(body)
}

/// Open or update the branch's pull request, once it is pushed and the workflow allows it.
async fn pull_request(
    workspace: Workspace,
    request: PullRequestRequest,
    orchestrator: &super::orchestrator::Orchestrator,
) -> Result<serde_json::Value, String> {
    let policy = workspace.workflow.lock().await.pr_policy();
    let rules = policy
        .as_ref()
        .map(|policy| policy.rules.clone())
        .unwrap_or_default();
    if rules.upstream == Some(Upstream::Never)
        && let Some(policy) = &policy
    {
        return Err(format!(
            "The \"{}\" workflow never upstreams: leave pushing and pull requests to the user.",
            policy.workflow
        ));
    }
    let place = place(workspace.cwd.clone(), None).await?;
    let (branch, remote) = (&place.branch, &place.remote);
    let head = place.head.clone().ok_or("HEAD has no commit yet")?;
    match place
        .forge
        .branch_head(branch)
        .await
        .map_err(|error| error.to_string())?
    {
        Some(pushed) if pushed == head => {}
        Some(pushed) => {
            return Err(format!(
                "GitHub has `{branch}` at {} but HEAD is {}: push first (`git push {remote} {branch}`).",
                short(&pushed),
                short(&head)
            ));
        }
        None => {
            return Err(format!(
                "`{branch}` is not on GitHub yet: push it first (`git push -u {remote} {branch}`)."
            ));
        }
    }

    if let Some(policy) = &policy
        && !policy.before_pr.is_empty()
        && workspace.workflow.lock().await.hold_pr(&head)
    {
        return Ok(json!({
            "opened": false,
            "workflow": policy.workflow,
            "before_opening": policy.before_pr,
            "then": "Do this first, then call pull_request again.",
        }));
    }

    let base = request
        .base
        .clone()
        .or_else(|| rules.base.clone())
        .or_else(|| place.default_branch.clone())
        .unwrap_or_else(|| "main".to_owned());
    if &base == branch {
        return Err(format!(
            "`{branch}` cannot merge into itself: work on another branch."
        ));
    }
    let broken = broken_rules(&place, &base, &request.body, &rules).await;
    if !broken.is_empty() {
        let name = policy
            .as_ref()
            .map_or("", |policy| policy.workflow.as_str());
        return Err(format!(
            "The \"{name}\" workflow's pull request rules are not met:\n- {}\nFix this, then call pull_request again.",
            broken.join("\n- ")
        ));
    }

    let draft = request.draft.unwrap_or(rules.draft);
    let existing = place
        .forge
        .pull_for_branch(branch)
        .await
        .map_err(|error| error.to_string())?
        .filter(|pull| pull.state == nori_forge::PullState::Open);
    // The workflow's `upstream-when` is the user's standing answer; without one, ask as the
    // session's approval policy says.
    let ask = match rules.upstream {
        Some(Upstream::Ask) => true,
        Some(Upstream::TestsPass | Upstream::Done) => false,
        Some(Upstream::Never) | None => orchestrator.asks_for_approval(),
    };
    if ask {
        let slug = place.forge.repo().slug();
        let (title, detail) = match &existing {
            Some(pull) => (
                format!("Update pull request #{}: {}", pull.number, request.title),
                format!("{slug}: {branch} → {}\n\n{}", pull.base, request.body),
            ),
            None => (
                format!("Open pull request: {}", request.title),
                format!(
                    "{slug}: {branch} → {base}{}\n\n{}",
                    if draft { " (draft)" } else { "" },
                    request.body
                ),
            ),
        };
        if !orchestrator.ask_user(title, detail).await {
            return Err("The user did not allow it.".to_owned());
        }
    }
    let (pull, created) = match existing {
        Some(pull) => (
            place
                .forge
                .update_pull(pull.number, &request.title, &request.body)
                .await,
            false,
        ),
        None => (
            place
                .forge
                .open_pull(&NewPull {
                    title: request.title,
                    body: request.body,
                    head: branch.clone(),
                    base,
                    draft,
                })
                .await,
            true,
        ),
    };
    let pull = pull.map_err(|error| error.to_string())?;
    Ok(json!({ "created": created, "pull_request": pull }))
}

/// The workflow's pull request rules that `body` and the branch's commits since `base` break.
async fn broken_rules(
    place: &Place,
    base: &str,
    body: &str,
    rules: &nori_workflow::PrRules,
) -> Vec<String> {
    let mut broken: Vec<String> = missing_sections(body, &rules.sections)
        .into_iter()
        .map(|section| format!("The description has no \"{section}\" section."))
        .collect();
    if rules.max_lines.is_none() && rules.max_files.is_none() {
        return broken;
    }
    let cwd = place.cwd.clone();
    let upstream_base = format!("{}/{base}", place.remote);
    let stats = tokio::task::spawn_blocking(move || {
        let repo = nori_config::GitRepo::discover(&cwd)?;
        let from = repo
            .merge_base("HEAD", &upstream_base)
            .unwrap_or(upstream_base);
        repo.range_stats(&from, "HEAD")
    })
    .await
    .ok()
    .flatten();
    let Some(stats) = stats else {
        return broken;
    };
    let lines = stats.added + stats.removed;
    if let Some(max) = rules.max_lines
        && lines > max
    {
        broken.push(format!(
            "It changes {lines} lines; at most {max} are allowed: split the work into a series of pull requests."
        ));
    }
    if let Some(max) = rules.max_files
        && stats.files > max
    {
        broken.push(format!(
            "It touches {} files; at most {max} are allowed: split the work into a series of pull requests.",
            stats.files
        ));
    }
    broken
}

/// The `sections` that `body` has no heading for. A heading is a line that reads as the
/// section's name once Markdown marks and a trailing colon are taken off: `## Why`, `**Why**`
/// and `Why:` all count.
fn missing_sections<'a>(body: &str, sections: &'a [String]) -> Vec<&'a str> {
    let headings: Vec<String> = body
        .lines()
        .map(|line| {
            line.trim()
                .trim_start_matches(['#', '*', '_', ' '])
                .trim_end_matches([':', '*', '_', ' '])
                .to_lowercase()
        })
        .collect();
    sections
        .iter()
        .map(String::as_str)
        .filter(|section| !headings.contains(&section.to_lowercase()))
        .collect()
}

fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

#[cfg(test)]
#[path = "forge_tools_tests.rs"]
mod tests;
