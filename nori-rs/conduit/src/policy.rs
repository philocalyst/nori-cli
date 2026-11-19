//! Deterministic sandbox, hierarchical token budget, and Bash AST guard enforcement.

use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;

use nori_bash::PathContext;
use nori_bash::ScriptFacts;

use crate::error::ConduitError;
use crate::schemas::SandboxPolicy;

/// Narrow an active sandbox with a nested `Op::Sandbox` scope.
/// Nested sandboxes intersect non-empty allowlists so inner scopes never widen access.
pub fn narrow_sandbox(outer: Option<&SandboxPolicy>, inner: &SandboxPolicy) -> SandboxPolicy {
    let Some(outer) = outer else {
        return inner.clone();
    };
    let allowed_tools = intersect_allowlists(&outer.allowed_tools, &inner.allowed_tools);
    let allowed_paths = intersect_path_allowlists(&outer.allowed_paths, &inner.allowed_paths);
    SandboxPolicy {
        allowed_tools,
        allowed_paths,
    }
}

fn intersect_allowlists(outer: &[String], inner: &[String]) -> Vec<String> {
    if outer.is_empty() {
        return inner.to_vec();
    }
    if inner.is_empty() {
        return outer.to_vec();
    }
    inner
        .iter()
        .filter(|item| outer.iter().any(|allowed| allowed == *item))
        .cloned()
        .collect()
}

fn intersect_path_allowlists(outer: &[String], inner: &[String]) -> Vec<String> {
    if outer.is_empty() {
        return inner.to_vec();
    }
    if inner.is_empty() {
        return outer.to_vec();
    }
    let root = Path::new("/");
    inner
        .iter()
        .filter(|candidate| {
            let norm_inner = normalize_lexical(root, candidate);
            outer.iter().any(|allowed| {
                let norm_outer = normalize_lexical(root, allowed);
                norm_inner.starts_with(&norm_outer)
            })
        })
        .cloned()
        .collect()
}

/// Verify that `tool_name` is permitted by `sandbox`.
pub fn enforce_sandbox_tool(
    sandbox: Option<&SandboxPolicy>,
    tool_name: &str,
) -> Result<(), ConduitError> {
    let Some(policy) = sandbox else {
        return Ok(());
    };
    if policy.allowed_tools.is_empty() {
        return Ok(());
    }
    let allowed = policy.allowed_tools.iter().any(|entry| {
        entry == "*" || entry == tool_name || (tool_name == "bash" && entry.starts_with("bash:"))
    });
    if allowed {
        Ok(())
    } else {
        Err(ConduitError::SandboxViolation {
            reason: format!(
                "tool `{tool_name}` is not permitted by sandbox allowlist {:?}",
                policy.allowed_tools
            ),
        })
    }
}

/// Verify that `candidate` resolves inside one of `sandbox.allowed_paths` when restricted.
pub fn enforce_sandbox_path(
    sandbox: Option<&SandboxPolicy>,
    workspace_root: &Path,
    candidate: &str,
) -> Result<(), ConduitError> {
    let Some(policy) = sandbox else {
        return Ok(());
    };
    enforce_allowed_paths(&policy.allowed_paths, workspace_root, candidate)
}

fn enforce_allowed_paths(
    allowed_paths: &[String],
    workspace_root: &Path,
    candidate: &str,
) -> Result<(), ConduitError> {
    if allowed_paths.is_empty() {
        return Ok(());
    }
    let resolved = normalize_lexical(workspace_root, candidate);
    let permitted = allowed_paths.iter().any(|allowed| {
        let base = normalize_lexical(workspace_root, allowed);
        resolved.starts_with(&base)
    });
    if permitted {
        Ok(())
    } else {
        Err(ConduitError::SandboxViolation {
            reason: format!(
                "path `{candidate}` ({}) is outside sandbox allowed_paths {allowed_paths:?}",
                resolved.display()
            ),
        })
    }
}

/// Inspect a Bash script via `nori_bash::inspect_script` and enforce `SandboxPolicy`
/// (`bash:<cmd>` command restrictions, redirection targets, and referenced paths).
pub fn enforce_bash_sandbox(
    sandbox: Option<&SandboxPolicy>,
    workspace_root: &Path,
    script: &str,
) -> Result<ScriptFacts, ConduitError> {
    let facts =
        nori_bash::inspect_script(script).map_err(|error| ConduitError::SandboxViolation {
            reason: format!("bash syntax rejected by AST inspector: {error}"),
        })?;
    let Some(policy) = sandbox else {
        return Ok(facts);
    };
    enforce_sandbox_tool(Some(policy), "bash")?;

    let scoped_commands: Vec<&str> = policy
        .allowed_tools
        .iter()
        .filter_map(|entry| entry.strip_prefix("bash:"))
        .collect();
    if !scoped_commands.is_empty() && !policy.allowed_tools.iter().any(|t| t == "bash" || t == "*")
    {
        if facts.has_dynamic_commands {
            return Err(ConduitError::SandboxViolation {
                reason: "dynamic bash command invocation is not permitted by scoped allowlist"
                    .to_owned(),
            });
        }
        for cmd in &facts.commands {
            let basename = Path::new(&cmd.name)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(&cmd.name);
            if !scoped_commands
                .iter()
                .any(|allowed| *allowed == "*" || *allowed == cmd.name || *allowed == basename)
            {
                return Err(ConduitError::SandboxViolation {
                    reason: format!(
                        "bash command `{}` is not permitted by scoped allowlist {scoped_commands:?}",
                        cmd.name
                    ),
                });
            }
        }
    }

    if !policy.allowed_paths.is_empty() {
        for redir in &facts.redirections {
            if !redir.target.starts_with('&') && redir.target != "/dev/null" {
                enforce_allowed_paths(&policy.allowed_paths, workspace_root, &redir.target)?;
            }
        }
        for lit in &facts.path_literals {
            if lit.context != PathContext::Command && lit.path != "/dev/null" {
                enforce_allowed_paths(&policy.allowed_paths, workspace_root, &lit.path)?;
            }
        }
    }
    Ok(facts)
}

fn normalize_lexical(workspace_root: &Path, raw: &str) -> PathBuf {
    let candidate = Path::new(raw);
    let combined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        workspace_root.join(candidate)
    };
    let mut normalized = PathBuf::new();
    for comp in combined.components() {
        match comp {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(comp.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
}

/// Thread-safe hierarchical token budget frame supporting nested `Op::Budget` scopes
/// and concurrent `Par`/`FanOut` branches.
#[derive(Debug)]
pub struct BudgetFrame {
    limit: i64,
    spent: AtomicI64,
    parent: Option<Arc<Self>>,
}

impl BudgetFrame {
    pub fn root(limit: i64) -> Arc<Self> {
        Arc::new(Self {
            limit: limit.max(0),
            spent: AtomicI64::new(0),
            parent: None,
        })
    }

    pub fn child(parent: Option<Arc<Self>>, limit: i64) -> Arc<Self> {
        Arc::new(Self {
            limit: limit.max(0),
            spent: AtomicI64::new(0),
            parent,
        })
    }

    pub fn spent(&self) -> i64 {
        self.spent.load(Ordering::Acquire)
    }

    pub fn limit(&self) -> i64 {
        self.limit
    }

    pub fn charge(&self, tokens: i64) -> Result<(), ConduitError> {
        let requested = tokens.max(0);
        if requested == 0 {
            return Ok(());
        }
        let mut current = self.spent.load(Ordering::Acquire);
        loop {
            let next = current.saturating_add(requested);
            if next > self.limit {
                return Err(ConduitError::BudgetExceeded {
                    limit: self.limit,
                    spent: current,
                    requested,
                });
            }
            match self.spent.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
        if let Some(parent) = &self.parent
            && let Err(error) = parent.charge(requested)
        {
            self.spent.fetch_sub(requested, Ordering::AcqRel);
            return Err(error);
        }
        Ok(())
    }
}
