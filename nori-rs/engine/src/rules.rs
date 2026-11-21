//! Permission rules: an action on a resource is allowed, asked about or denied.
//!
//! A flavor states its base rules (from its own configuration), a mode adds its own on top, and
//! a person's "always allow" answers add more during the session. The last rule that matches
//! decides. Actions are permission names (`edit`, `shell`, `read`, `external_directory`, a tool's
//! own name, `*`); resources are globs over a path or a command.

/// What a rule decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    Allow,
    Ask,
    Deny,
}

/// One rule: `action` on resources matching `resource`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub action: String,
    pub resource: String,
    pub effect: Effect,
}

impl Rule {
    pub fn new(action: &str, resource: &str, effect: Effect) -> Self {
        Self {
            action: action.to_owned(),
            resource: resource.to_owned(),
            effect,
        }
    }

    fn matches(&self, action: &str, resource: &str) -> bool {
        wildmatch::WildMatch::new(&self.action).matches(action)
            && wildmatch::WildMatch::new(&self.resource).matches(resource)
    }
}

/// What `rules` decide for `action` on `resource`; `fallback` when none matches.
pub fn evaluate(rules: &[Rule], action: &str, resource: &str, fallback: Effect) -> Effect {
    rules
        .iter()
        .rev()
        .find(|rule| rule.matches(action, resource))
        .map_or(fallback, |rule| rule.effect)
}

/// Whether `rules` deny `action` everywhere, so a tool for it is not offered at all.
pub fn denied_everywhere(rules: &[Rule], action: &str) -> bool {
    rules
        .iter()
        .rev()
        .find(|rule| rule.resource == "*" && rule.matches(action, "*"))
        .is_some_and(|rule| rule.effect == Effect::Deny)
}

/// The permission a tool's calls are checked against when they do not say more.
pub fn primary_action(tool: &str) -> &str {
    match tool {
        "write" | "edit" | "patch" | "apply_patch" | "multiedit" | "multi_edit" => "edit",
        "bash" | "shell" | "shell_command" => "shell",
        "task" | "delegate_task" => "subagent",
        other => other,
    }
}
