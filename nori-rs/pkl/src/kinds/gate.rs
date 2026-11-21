//! The `nori:Gate` kind: ordered rules consulted synchronously before something happens.
//!
//! These are the types a gate module evaluates to. The harness compiles them once into matchers
//! (`nori-harness`, `backend::gates`); everything a compile could reject (a regular expression
//! or a glob that does not parse, a decision without its reason) is caught here by
//! [`GateDef::validate`], so a module that loads always compiles.

use std::collections::BTreeMap;

use globset::Glob;
use globset::GlobBuilder;
use globset::GlobSet;
use globset::GlobSetBuilder;
use serde::Deserialize;
use serde::Serialize;

use super::check_name;
use super::check_regex;
use super::finish;

/// What a gate listens to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateEvent {
    /// An agent asked the person for permission.
    Permission,
    /// An agent called one of Nori's own tools.
    HostTool,
    /// The agent ended its turn.
    Stop,
}

impl GateEvent {
    /// The event's name in a module and in the journal.
    pub fn as_str(self) -> &'static str {
        match self {
            GateEvent::Permission => "permission",
            GateEvent::HostTool => "host_tool",
            GateEvent::Stop => "stop",
        }
    }
}

/// A regular expression a text must match, or must not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Pattern {
    /// The text must match.
    Matches(String),
    /// The text must not match.
    Not(Not),
}

/// A regular expression the text must not match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Not {
    /// The regular expression.
    pub regex: String,
}

impl Pattern {
    /// The regular expression, and whether a match is wanted.
    pub fn parts(&self) -> (&str, bool) {
        match self {
            Pattern::Matches(regex) => (regex, true),
            Pattern::Not(Not { regex }) => (regex, false),
        }
    }
}

/// The kinds of tool an agent's permission request can be for, as ACP names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    /// Reading files.
    Read,
    /// Editing files.
    Edit,
    /// Deleting files.
    Delete,
    /// Moving or renaming files.
    Move,
    /// Searching.
    Search,
    /// Running a command.
    Execute,
    /// Thinking.
    Think,
    /// Fetching a URL.
    Fetch,
    /// Switching the session's mode.
    SwitchMode,
    /// Anything else.
    Other,
}

/// What a `permission` rule decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    /// Approve without asking the person.
    Allow,
    /// Refuse without asking the person.
    Deny,
    /// Always ask the person.
    Ask,
}

/// What a `host_tool` rule decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostToolDecision {
    /// Run the call as it is.
    Allow,
    /// Refuse the call.
    Deny,
    /// Run the call with other arguments.
    Rewrite,
}

/// What a `stop` rule decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopDecision {
    /// Hand the agent a hidden prompt so the turn goes on.
    Continue,
    /// Let the turn end.
    Pass,
}

/// One rule of a gate. Which kind is valid depends on the gate's event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Rule {
    /// A rule of a `permission` gate.
    Permission {
        /// The kinds of tool; any when empty.
        #[serde(default)]
        kinds: Vec<ToolKind>,
        /// What the request is titled.
        #[serde(default)]
        title: Option<Pattern>,
        /// The command the call runs.
        #[serde(default)]
        command: Option<Pattern>,
        /// At least one file the call names matches one of these.
        #[serde(default)]
        paths: Vec<String>,
        /// Every file the call names matches one of these.
        #[serde(default)]
        only_paths: Vec<String>,
        /// What to do.
        decision: PermissionDecision,
        /// With `deny`: what the agent is told.
        #[serde(default)]
        reason: Option<String>,
    },
    /// A rule of a `host_tool` gate.
    HostTool {
        /// The tools by name; any when empty.
        #[serde(default)]
        tools: Vec<String>,
        /// What arguments must match, by name.
        #[serde(default)]
        arguments: BTreeMap<String, Pattern>,
        /// Arguments the call must not pass.
        #[serde(default)]
        absent: Vec<String>,
        /// What to do.
        decision: HostToolDecision,
        /// With `deny`: the tool's error.
        #[serde(default)]
        reason: Option<String>,
        /// With `rewrite`: arguments to set.
        #[serde(default)]
        set: BTreeMap<String, serde_json::Value>,
        /// With `rewrite`: text to add after string arguments.
        #[serde(default)]
        append: BTreeMap<String, String>,
    },
    /// A rule of a `stop` gate.
    Stop {
        /// What the agent's last message says.
        #[serde(default)]
        last_message: Option<Pattern>,
        /// The turn changed a file matching one of these.
        #[serde(default)]
        changed: Vec<String>,
        /// What to do.
        decision: StopDecision,
        /// With `continue`: the hidden prompt.
        #[serde(default)]
        message: Option<String>,
        /// With `continue`: how many times for one prompt of the person.
        max_continues: i64,
    },
}

impl Rule {
    /// The event this rule belongs to.
    pub fn event(&self) -> GateEvent {
        match self {
            Rule::Permission { .. } => GateEvent::Permission,
            Rule::HostTool { .. } => GateEvent::HostTool,
            Rule::Stop { .. } => GateEvent::Stop,
        }
    }
}

/// A gate module: when it is consulted and the rules it applies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateDef {
    /// The gate's name (`[a-z0-9][a-z0-9_-]*`).
    pub name: String,
    /// One line saying what the gate is for.
    pub description: String,
    /// The event it listens to.
    pub on: GateEvent,
    /// The rules in order; the first that matches decides.
    pub rules: Vec<Rule>,
}

/// The globs of a rule, as one matcher over project-relative paths: `*` stays within a directory,
/// `**` crosses them. The one place the dialect is decided, for validation and for the harness's
/// compile.
pub fn glob_set(globs: &[String]) -> Result<GlobSet, globset::Error> {
    let mut set = GlobSetBuilder::new();
    for glob in globs {
        set.add(glob_of(glob)?);
    }
    set.build()
}

fn glob_of(glob: &str) -> Result<Glob, globset::Error> {
    GlobBuilder::new(glob).literal_separator(true).build()
}

fn check_globs(field: &str, globs: &[String], problems: &mut Vec<String>) {
    for (index, glob) in globs.iter().enumerate() {
        if let Err(error) = glob_of(glob) {
            problems.push(format!(
                "{field}[{index}]: {glob:?} is not a valid glob ({})",
                error.kind()
            ));
        }
    }
}

fn check_pattern(field: &str, pattern: &Option<Pattern>, problems: &mut Vec<String>) {
    if let Some(pattern) = pattern {
        check_regex(field, pattern.parts().0, problems);
    }
}

impl GateDef {
    /// Checks what the types cannot: the name, that rules belong to the gate's event, that every
    /// regular expression and glob parses, and that each decision has what it needs (a refusal its
    /// reason, a rewrite something to change, a continuation its message). Every message names
    /// the field.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut problems = Vec::new();
        check_name("name", &self.name, &mut problems);
        if self.rules.is_empty() {
            problems.push("rules: a gate needs at least one rule".to_owned());
        }
        for (index, rule) in self.rules.iter().enumerate() {
            let at = format!("rules[{index}]");
            if rule.event() != self.on {
                problems.push(format!(
                    "{at}: a {} rule in a gate on \"{}\"",
                    rule.event().as_str(),
                    self.on.as_str()
                ));
                continue;
            }
            match rule {
                Rule::Permission {
                    title,
                    command,
                    paths,
                    only_paths,
                    decision,
                    reason,
                    ..
                } => {
                    check_pattern(&format!("{at}.title"), title, &mut problems);
                    check_pattern(&format!("{at}.command"), command, &mut problems);
                    check_globs(&format!("{at}.paths"), paths, &mut problems);
                    check_globs(&format!("{at}.only_paths"), only_paths, &mut problems);
                    if *decision == PermissionDecision::Deny && is_blank(reason) {
                        problems.push(format!(
                            "{at}.reason: a deny must say what the agent should do instead"
                        ));
                    }
                }
                Rule::HostTool {
                    arguments,
                    decision,
                    reason,
                    set,
                    append,
                    ..
                } => {
                    for (name, pattern) in arguments {
                        check_regex(
                            &format!("{at}.arguments[{name:?}]"),
                            pattern.parts().0,
                            &mut problems,
                        );
                    }
                    match decision {
                        HostToolDecision::Deny if is_blank(reason) => problems.push(format!(
                            "{at}.reason: a deny must say what the agent should do instead"
                        )),
                        HostToolDecision::Rewrite if set.is_empty() && append.is_empty() => {
                            problems.push(format!(
                                "{at}: a rewrite needs `set` or `append` to change something"
                            ));
                        }
                        HostToolDecision::Allow
                        | HostToolDecision::Deny
                        | HostToolDecision::Rewrite => {}
                    }
                }
                Rule::Stop {
                    last_message,
                    changed,
                    decision,
                    message,
                    max_continues,
                } => {
                    check_pattern(&format!("{at}.last_message"), last_message, &mut problems);
                    check_globs(&format!("{at}.changed"), changed, &mut problems);
                    if *decision == StopDecision::Continue && is_blank(message) {
                        problems.push(format!("{at}.message: a continue needs its prompt"));
                    }
                    if *max_continues <= 0 {
                        problems.push(format!(
                            "{at}.max_continues: must be greater than zero, got {max_continues}"
                        ));
                    }
                }
            }
        }
        finish(problems)
    }
}

fn is_blank(text: &Option<String>) -> bool {
    text.as_deref().is_none_or(|text| text.trim().is_empty())
}
