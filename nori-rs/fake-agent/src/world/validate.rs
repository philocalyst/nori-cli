//! Checking a world before anything runs, so a typo is an error that names the step and not a
//! scene that silently never matches.

use std::collections::HashSet;

use super::AgentDef;
use super::ConfigEffect;
use super::ErrorClass;
use super::Step;
use super::Text;
use super::World;

/// Largest generated text a script may ask for, in repetitions: a guard against a typo, not a
/// limit on huge outputs (ten million lines is already huge).
const MAX_GENERATED_REPEATS: i64 = 10_000_000;

struct Problems<'a> {
    world: &'a World,
    found: Vec<String>,
}

impl Problems<'_> {
    fn add(&mut self, at: &str, message: impl std::fmt::Display) {
        self.found.push(format!("{at}: {message}"));
    }

    fn text(&mut self, at: &str, text: &Text) {
        if let Text::Generated { times, .. } = text
            && !(0..=MAX_GENERATED_REPEATS).contains(times)
        {
            self.add(
                at,
                format_args!(
                    "generated text repeats {times} times; use 0 to {MAX_GENERATED_REPEATS}"
                ),
            );
        }
    }

    fn steps(&mut self, at: &str, steps: &[Step]) {
        for (index, step) in steps.iter().enumerate() {
            self.step(&format!("{at} step {}", index + 1), step);
        }
    }

    fn step(&mut self, at: &str, step: &Step) {
        match step {
            Step::Say(say) | Step::Think(say) => {
                self.text(at, &say.text);
                if say.chunk.size < 0 {
                    self.add(at, "chunk.size must not be negative; 0 sends one chunk");
                }
            }
            Step::Tool(tool) => {
                if tool.title.is_empty() {
                    self.add(at, "a tool needs a title");
                }
            }
            Step::ToolStart(start) => {
                if start.id.is_empty() {
                    self.add(at, "tool_start needs the id later steps will update");
                }
            }
            Step::ToolUpdate(update) => {
                if update.id.is_empty() {
                    self.add(at, "tool_update needs the id of the call to update");
                }
            }
            Step::Permission(permission) => {
                if permission.tool.title.is_empty() {
                    self.add(at, "a permission request needs the tool's title");
                }
                let mut ids = HashSet::new();
                for option in &permission.options {
                    if !ids.insert(option.id.as_str()) {
                        self.add(at, format_args!("option id {:?} is repeated", option.id));
                    }
                }
                if !permission.options.is_empty() {
                    let allows = permission.options.iter().any(|option| option.kind.allows());
                    let denies = permission
                        .options
                        .iter()
                        .any(|option| !option.kind.allows());
                    if !(allows && denies) {
                        self.add(at, "options need at least one allow and one reject kind");
                    }
                }
                self.steps(&format!("{at} > on_allow"), &permission.on_allow);
                self.steps(&format!("{at} > on_deny"), &permission.on_deny);
                self.steps(&format!("{at} > on_cancelled"), &permission.on_cancelled);
                self.steps(&format!("{at} > on_timeout"), &permission.on_timeout);
                if !permission.on_timeout.is_empty() && permission.timeout.is_none() {
                    self.add(at, "on_timeout is set but timeout is not");
                }
            }
            Step::HostTool(call) => {
                if call.name.is_empty() {
                    self.add(at, "host_tool needs the tool's name");
                }
            }
            Step::Delegate(delegate) => {
                self.text(at, &delegate.task);
                if delegate.task.is_empty() {
                    self.add(at, "delegate needs a task: the child sees nothing else");
                }
                if let Some(agent) = &delegate.agent
                    && self.world.agent(agent).is_none()
                {
                    self.add(
                        at,
                        format_args!(
                            "delegates to agent {agent:?}, which this world does not define; it defines: {}",
                            self.world.slugs().join(", ")
                        ),
                    );
                }
            }
            Step::Send(send) => {
                if send.to.is_empty() {
                    self.add(at, "send needs a recipient (an alias, `actor://alias`, a session id or `task-N`)");
                }
                self.text(at, &send.message);
            }
            Step::Error(error) => {
                if error.failure == ErrorClass::Raw
                    && error.code.is_none()
                    && error.message.is_none()
                {
                    self.add(at, "a raw error needs a code or a message");
                }
            }
            Step::Detach(detach) => self.steps(&format!("{at} > detach"), &detach.steps),
            Step::Par(par) => {
                if par.lanes.is_empty() {
                    self.add(at, "par needs at least one lane");
                }
                for (index, lane) in par.lanes.iter().enumerate() {
                    self.steps(&format!("{at} > lane {}", index + 1), lane);
                }
            }
            Step::Lane(lane) => {
                if lane.session.is_empty() {
                    self.add(
                        at,
                        "lane needs a session: self, previous, first, newest or an id",
                    );
                }
                self.steps(&format!("{at} > lane"), &lane.steps);
            }
            Step::Subagent(subagent) => {
                if subagent.title.is_empty() {
                    self.add(at, "a subagent needs a title");
                }
                self.steps(&format!("{at} > subagent"), &subagent.steps);
            }
            Step::ConfigUpdate(update) => {
                let mut ids = HashSet::new();
                for model in &update.models {
                    if !ids.insert(model.id.as_str()) {
                        self.add(at, format_args!("model {:?} is listed twice", model.id));
                    }
                }
                if update.models.is_empty() && update.set.is_empty() {
                    self.add(
                        at,
                        "config_update needs the models the agent announces, or options it `set`s",
                    );
                }
            }
            Step::PeerMessage(message) => self.text(at, &message.text),
            Step::When(when) => {
                let conditions = [
                    when.matches.is_some(),
                    when.equals.is_some(),
                    when.bound.is_some(),
                ];
                if conditions.into_iter().filter(|set| *set).count() != 1 {
                    self.add(at, "when needs exactly one of matches, equals and bound");
                }
                if let Some(pattern) = &when.matches
                    && let Err(error) = regex::Regex::new(pattern)
                {
                    self.add(
                        at,
                        format_args!("matches {pattern:?} is not a regular expression: {error}"),
                    );
                }
                self.steps(&format!("{at} > then"), &when.then);
                self.steps(&format!("{at} > otherwise"), &when.otherwise);
            }
            Step::Set(set) => {
                if set.name.is_empty() {
                    self.add(at, "set needs the name of the variable to bind");
                }
                self.text(at, &set.value);
            }
            Step::Commands(commands) => {
                if commands
                    .commands
                    .iter()
                    .any(|command| command.name.is_empty())
                {
                    self.add(at, "a command needs a name");
                }
            }
            Step::Repeat(repeat) => {
                if repeat.times < 0 {
                    self.add(at, "repeat.times must not be negative");
                }
                self.steps(&format!("{at} > repeat"), &repeat.steps);
            }
            Step::Wait(_)
            | Step::Plan(_)
            | Step::Usage(_)
            | Step::SessionInfo(_)
            | Step::Status(_)
            | Step::Mode(_)
            | Step::Stop(_)
            | Step::Hang
            | Step::DropResponse
            | Step::RespondTwice(_)
            | Step::StaleResponse(_)
            | Step::Crash(_)
            | Step::WireNoise(_)
            | Step::CloseStdout
            | Step::Throttle(_)
            | Step::StderrFlood(_)
            | Step::RawUpdate(_) => {}
        }
    }

    fn agent(&mut self, agent: &AgentDef) {
        let at = format!("agent {:?}", agent.slug);
        let valid_slug = !agent.slug.is_empty()
            && agent
                .slug
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
        if !valid_slug {
            self.add(&at, "a slug is lowercase letters, digits, `-` and `_`");
        }
        let mut models = HashSet::new();
        for model in &agent.models {
            if !models.insert(model.id.as_str()) {
                self.add(&at, format_args!("model {:?} is listed twice", model.id));
            }
        }
        if let Some(default) = &agent.default_model
            && !models.contains(default.as_str())
        {
            self.add(
                &at,
                format_args!("default_model {default:?} is not among its models"),
            );
        }
        if let Some(default) = &agent.default_mode
            && !agent.modes.iter().any(|mode| &mode.id == default)
        {
            self.add(
                &at,
                format_args!("default_mode {default:?} is not among its modes"),
            );
        }
        for (name, limit) in [
            (
                "limits.max_text_block_bytes",
                agent.limits.max_text_block_bytes,
            ),
            ("limits.max_prompt_bytes", agent.limits.max_prompt_bytes),
        ] {
            if limit.is_some_and(|bytes| bytes <= 0) {
                self.add(&at, format_args!("{name} must be positive"));
            }
        }
        for (index, fault) in agent.config_faults.iter().enumerate() {
            let at = format!("{at} config_fault {}", index + 1);
            if fault.option.is_empty() {
                self.add(
                    &at,
                    "needs the option id it misbehaves for (`model`, `thought_level`, ...)",
                );
            }
            match fault.effect {
                ConfigEffect::Substitute if fault.actual.is_none() => {
                    self.add(&at, "substitute needs `actual`, the value the option takes");
                }
                ConfigEffect::Reject | ConfigEffect::Ignore | ConfigEffect::Substitute => {}
            }
        }
        if agent.startup.models_announced_after.is_some() && agent.models.is_empty() {
            self.add(
                &at,
                "startup.models_announced_after announces the agent's `models`, and it has none",
            );
        }
        if agent.startup.requires_auth && agent.startup.new_session_error.is_some() {
            self.add(&at, "startup.requires_auth and startup.new_session_error both fail session/new; pick one");
        }
        for (index, scene) in agent.scenes.iter().enumerate() {
            let label = scene
                .name
                .as_deref()
                .map_or_else(String::new, |name| format!(" {name:?}"));
            let at = format!("{at} scene {}{label}", index + 1);
            for message in scene.on.problems() {
                self.add(&at, message);
            }
            if scene.times.is_some_and(|times| times <= 0) {
                self.add(
                    &at,
                    "times must be positive; a scene that answers nothing is a scene to remove",
                );
            }
            if let Some(model) = &scene.model
                && !agent.models.is_empty()
                && !models.contains(model.as_str())
            {
                self.add(
                    &at,
                    format_args!("model {model:?} is not one of this agent's models"),
                );
            }
            self.steps(&at, &scene.steps);
            self.steps(&format!("{at} > cancel.then"), &scene.cancel.then);
        }
    }
}

impl World {
    /// Every problem in the world, each with where it is, or `Ok`.
    pub fn validate(&self) -> Result<(), String> {
        let mut problems = Problems {
            world: self,
            found: Vec::new(),
        };
        if self.agents.is_empty() {
            problems.add("world", "defines no agents");
        }
        let mut slugs = HashSet::new();
        for agent in &self.agents {
            if !slugs.insert(agent.slug.as_str()) {
                problems.add(
                    "world",
                    format_args!("agent {:?} is defined twice", agent.slug),
                );
            }
        }
        match &self.default_agent {
            Some(slug) if self.agent(slug).is_none() => problems.add(
                "world",
                format_args!(
                    "default_agent {slug:?} is not defined; it defines: {}",
                    self.slugs().join(", ")
                ),
            ),
            None if self.agents.len() > 1 => problems.add(
                "world",
                "has several agents and no default_agent; say which one a session starts on",
            ),
            Some(_) | None => {}
        }
        if crate::clock::parse_rfc3339_utc(&self.start).is_none() {
            problems.add(
                "world",
                format_args!(
                    "start {:?} is not an RFC 3339 UTC time such as 2026-11-18T12:00:00Z",
                    self.start
                ),
            );
        }
        if let Some(clock) = &self.clock
            && let Err(message) = crate::clock::ClockMode::parse(clock)
        {
            problems.add("world", message);
        }
        for agent in &self.agents {
            problems.agent(agent);
        }
        for (index, process) in self.processes.iter().enumerate() {
            if let Some(message) = process.pattern_error() {
                problems.add(&format!("process {}", index + 1), message);
            }
        }
        if problems.found.is_empty() {
            Ok(())
        } else {
            Err(problems.found.join("\n  "))
        }
    }
}
