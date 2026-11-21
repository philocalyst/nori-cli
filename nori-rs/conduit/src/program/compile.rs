//! Check a [`Program`] and compile it to an op graph.
//!
//! One pass walks the steps in execution order, tracking which names are bound, and collects
//! every problem it finds (not just the first) so an author fixes a program in one round.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use nori_pkl::recovery::RecoveryBlock;

use super::locate::Locator;
use super::model::Check;
use super::model::Program;
use super::model::Step;
use super::model::StepKind;
use super::model::StepMeta;
use super::template;
use crate::error::ConduitError;
use crate::error::Problem;
use crate::op::CompiledProgram;
use crate::op::Node;
use crate::op::Op;
use crate::op::Target;
use crate::op::ToolRef;
use crate::schemas::ToolSource;
use crate::schemas::ToolSpec;

const MAX_DEPTH: usize = 32;
const MAX_STEPS: i64 = 2000;

/// What is known to be in scope at a point of the program.
#[derive(Clone, Default)]
struct Names {
    /// Bindings that hold a value.
    bound: BTreeSet<String>,
    /// Bindings that hold the receipt of a send that did not await its reply.
    receipts: BTreeSet<String>,
}

impl Names {
    /// What is certainly bound after any one of `branches` ran.
    fn certain(branches: &[Self]) -> Self {
        let mut iter = branches.iter();
        let Some(first) = iter.next() else {
            return Self::default();
        };
        iter.fold(first.clone(), |acc, other| Self {
            bound: acc.bound.intersection(&other.bound).cloned().collect(),
            receipts: acc
                .receipts
                .intersection(&other.receipts)
                .cloned()
                .collect(),
        })
    }
}

struct Compiler<'a> {
    program: &'a Program,
    locator: &'a Locator,
    problems: Vec<Problem>,
    nodes: i64,
}

/// Check `program` and compile it. Every problem found is reported, with the source line of
/// the offending property when the module states it literally.
pub fn compile(program: &Program, locator: &Locator) -> Result<CompiledProgram, ConduitError> {
    let mut compiler = Compiler {
        program,
        locator,
        problems: Vec::new(),
        nodes: 0,
    };
    let compiled = compiler.program();
    if compiler.problems.is_empty() {
        Ok(compiled)
    } else {
        Err(ConduitError::InvalidProgram {
            file: locator.file().to_owned(),
            problems: compiler.problems,
        })
    }
}

impl Compiler<'_> {
    fn problem(&mut self, path: &str, hint: Option<(&str, &str)>, message: impl Into<String>) {
        self.problems.push(Problem {
            location: self.locator.at(path, hint),
            message: message.into(),
        });
    }

    fn recovery_block(&mut self, path: &str, block: Option<&RecoveryBlock>) {
        if let Some(Err(error)) = block.map(RecoveryBlock::to_override) {
            self.problem(path, None, format!("recovery: {error}"));
        }
    }

    fn program(&mut self) -> CompiledProgram {
        let program = self.program;
        self.recovery_block("recovery", program.recovery.as_ref());
        let mut tools: BTreeMap<&str, &ToolSpec> = BTreeMap::new();
        for (index, tool) in program.tools.iter().enumerate() {
            let path = format!("tools[{index}]");
            if tools.insert(&tool.name, tool).is_some() {
                self.problem(
                    &path,
                    Some(("name", &tool.name)),
                    format!("tool `{}` is declared twice", tool.name),
                );
            }
            if let ToolSource::Wasm {
                file: module,
                args_schema,
                ..
            } = &tool.source
            {
                let escapes = std::path::Path::new(module).is_absolute()
                    || module.split('/').any(|part| part == "..");
                if escapes {
                    self.problem(
                        &path,
                        Some(("file", module)),
                        format!(
                            "wasm module `{module}` must be a path inside the program's directory"
                        ),
                    );
                }
                if let Err(error) = serde_json::from_str::<serde_json::Value>(args_schema) {
                    self.problem(
                        &path,
                        Some(("name", &tool.name)),
                        format!("args_schema of `{}` is not JSON: {error}", tool.name),
                    );
                }
            }
        }
        for (alias, actor) in &program.actors {
            let path = format!("actors[\"{alias}\"]");
            self.recovery_block(&path, actor.recovery.as_ref());
            for toolset in &actor.toolsets {
                if !program
                    .toolsets
                    .iter()
                    .any(|declared| &declared.name == toolset)
                {
                    self.problem(
                        &path,
                        Some(("toolsets", toolset)),
                        format!(
                            "actor `{alias}` uses toolset `{toolset}`, which the program does not declare; declared toolsets: {}",
                            list(program.toolsets.iter().map(|declared| declared.name.as_str()))
                        ),
                    );
                }
            }
            if let Some(world) = &actor.world
                && !program.worlds.contains_key(world)
            {
                self.problem(
                    &path,
                    Some(("world", world)),
                    format!(
                        "actor `{alias}` lives in world `{world}`, which is not declared; declared worlds: {}",
                        list(program.worlds.keys())
                    ),
                );
            }
        }
        let mut names = Names::default();
        let steps = self.steps(&program.steps, "steps", &mut names, 1);
        if program.steps.is_empty() {
            self.problem("steps", None, "the program has no steps");
        }
        if let Some(result) = &program.result
            && !names.bound.contains(result)
        {
            self.problem(
                "result",
                Some(("result", result)),
                format!(
                    "`result` names `{result}`, which no step binds; bound names: {}",
                    list(names.bound.iter())
                ),
            );
        }
        CompiledProgram {
            name: program.name.clone(),
            description: program.description.clone(),
            actors: program.actors.clone(),
            worlds: program.worlds.clone(),
            world: program.world.clone(),
            toolsets: program.toolsets.clone(),
            tools: program.tools.clone(),
            recovery: program.recovery.clone(),
            budget_tokens: program.budget_tokens,
            root: Node {
                path: "steps".to_owned(),
                id: None,
                bind: None,
                recovery: None,
                timeout_ms: None,
                op: Op::Seq { body: steps },
            },
            result: program.result.clone(),
        }
    }

    fn steps(&mut self, steps: &[Step], path: &str, names: &mut Names, depth: usize) -> Vec<Node> {
        steps
            .iter()
            .enumerate()
            .map(|(index, step)| self.step(step, format!("{path}[{index}]"), names, depth))
            .collect()
    }

    /// A template's references must be bound; text with a malformed reference is a problem.
    fn template(&mut self, path: &str, field: &str, text: &str, names: &Names, extra: &[&str]) {
        match template::parse(text) {
            Err(error) => self.problem(&format!("{path}.{field}"), Some((field, text)), error),
            Ok(pieces) => {
                for piece in pieces {
                    if let template::Piece::Name(name) = piece
                        && !names.bound.contains(name)
                        && !extra.contains(&name)
                    {
                        let mut known: Vec<&str> = names.bound.iter().map(String::as_str).collect();
                        known.extend(extra);
                        self.problem(
                            &format!("{path}.{field}"),
                            Some((field, text)),
                            format!(
                                "`{{{{{name}}}}}` is not bound where this step runs; bound names: {}",
                                if known.is_empty() { "none".to_owned() } else { known.join(", ") }
                            ),
                        );
                    }
                }
            }
        }
    }

    fn check(&mut self, path: &str, check: &Check, names: &Names, extra: &[&str]) {
        let set = [&check.contains, &check.equals, &check.starts_with]
            .iter()
            .filter(|predicate| predicate.is_some())
            .count();
        if set != 1 {
            self.problem(
                path,
                Some(("value", &check.value)),
                format!(
                    "a check needs exactly one of `contains`, `equals` and `starts_with`; this one sets {set}"
                ),
            );
        }
        if !names.bound.contains(&check.value) && !extra.contains(&check.value.as_str()) {
            self.problem(
                path,
                Some(("value", &check.value)),
                format!(
                    "check reads `{}`, which is not bound here; bound names: {}",
                    check.value,
                    list(
                        names
                            .bound
                            .iter()
                            .map(String::as_str)
                            .chain(extra.iter().copied())
                    )
                ),
            );
        }
    }

    fn bind(&mut self, path: &str, bind: &Option<String>, names: &mut Names) {
        if let Some(name) = bind {
            if matches!(name.as_str(), "output" | "feedback") {
                self.problem(
                    path,
                    Some(("bind", name)),
                    format!("`{name}` is reserved for guards; choose another binding name"),
                );
            }
            names.bound.insert(name.clone());
        }
    }

    fn actor_target(&mut self, path: &str, field: &str, to: &str) -> Target {
        if let Some(external) = to.strip_prefix("actor://") {
            return Target::External(external.to_owned());
        }
        if !self.program.actors.contains_key(to) {
            self.problem(
                path,
                Some((field, to)),
                format!(
                    "unknown actor `{to}`; declared actors: {}; write `actor://{to}` to address an actor this program did not start",
                    list(self.program.actors.keys())
                ),
            );
        }
        Target::Declared(to.to_owned())
    }

    fn step(&mut self, step: &Step, path: String, names: &mut Names, depth: usize) -> Node {
        self.nodes += 1;
        if self.nodes == MAX_STEPS + 1 {
            self.problem(
                &path,
                None,
                format!("a program may have at most {MAX_STEPS} steps"),
            );
        }
        if depth > MAX_DEPTH {
            self.problem(
                &path,
                None,
                format!("steps nest deeper than {MAX_DEPTH} levels"),
            );
        }
        self.recovery_block(&path, step.meta.recovery.as_ref());
        let timeout_ms =
            step.meta
                .timeout
                .as_ref()
                .and_then(|timeout| match timeout.to_duration() {
                    Ok(duration) => Some(i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)),
                    Err(error) => {
                        self.problem(&path, None, format!("timeout: {error}"));
                        None
                    }
                });
        let op = self.op(&step.body, &step.meta, &path, names, depth);
        let waits_for_reply = !matches!(
            step.body,
            StepKind::Send {
                await_reply: false,
                ..
            }
        );
        let bind = step.meta.bind.clone().filter(|_| waits_for_reply);
        self.bind(&path, &bind, names);
        Node {
            path,
            id: step.meta.id.clone(),
            bind,
            recovery: step.meta.recovery.clone(),
            timeout_ms,
            op,
        }
    }

    fn op(
        &mut self,
        kind: &StepKind,
        meta: &StepMeta,
        path: &str,
        names: &mut Names,
        depth: usize,
    ) -> Op {
        let nested = depth + 1;
        match kind {
            StepKind::Prompt { text } => {
                self.template(path, "text", text, names, &[]);
                Op::Prompt { text: text.clone() }
            }
            StepKind::Config { key, value } => {
                self.template(path, "value", value, names, &[]);
                Op::Config {
                    key: key.clone(),
                    value: value.clone(),
                }
            }
            StepKind::Spawn { actor } => {
                if !self.program.actors.contains_key(actor) {
                    self.problem(
                        path,
                        Some(("actor", actor)),
                        format!(
                            "unknown actor `{actor}`; declared actors: {}",
                            list(self.program.actors.keys())
                        ),
                    );
                }
                Op::Spawn {
                    actor: actor.clone(),
                }
            }
            StepKind::Send {
                to,
                message,
                await_reply,
                delivery,
            } => {
                let target = self.actor_target(path, "to", to);
                self.template(path, "message", message, names, &[]);
                let receipt = meta.bind.clone().filter(|_| !*await_reply);
                if let Some(name) = &receipt {
                    names.receipts.insert(name.clone());
                }
                Op::Send {
                    to: target,
                    message: message.clone(),
                    wait: *await_reply,
                    delivery: *delivery,
                    receipt,
                }
            }
            StepKind::Await { receipt } => {
                if !names.receipts.contains(receipt) {
                    self.problem(
                        path,
                        Some(("receipt", receipt)),
                        format!(
                            "`{receipt}` is not the receipt of an earlier `Send` with `await_reply = false`; receipts in scope: {}",
                            list(names.receipts.iter())
                        ),
                    );
                }
                Op::Await {
                    receipt: receipt.clone(),
                }
            }
            StepKind::Call { tool, args } => {
                let tool_ref = match self.program.tools.iter().find(|spec| &spec.name == tool) {
                    Some(spec) => ToolRef::Declared {
                        tool: Box::new(spec.clone()),
                    },
                    None => ToolRef::Host { name: tool.clone() },
                };
                let mut texts = Vec::new();
                collect_strings(args, &mut texts);
                for text in texts {
                    self.template(path, "args", text, names, &[]);
                }
                Op::Call {
                    tool: tool_ref,
                    args: args.clone(),
                }
            }
            StepKind::Unsafe { program, args, cwd } => {
                for arg in args {
                    self.template(path, "args", arg, names, &[]);
                }
                Op::Exec {
                    program: program.clone(),
                    args: args.clone(),
                    cwd: cwd.clone(),
                }
            }
            StepKind::SelectContext { start, end } => {
                if start > end {
                    self.problem(path, None, format!("context range {start}..{end} is empty"));
                }
                Op::SelectContext {
                    start: *start,
                    end: *end,
                }
            }
            StepKind::Fork { actor, start, end } => {
                if start > end {
                    self.problem(path, None, format!("fork range {start}..{end} is empty"));
                }
                Op::Fork {
                    source: actor.clone(),
                    start: *start,
                    end: *end,
                }
            }
            StepKind::Rewind { head } => Op::Rewind { head: *head },
            StepKind::Sequence { steps } => {
                self.nonempty(path, "steps", steps);
                Op::Seq {
                    body: self.steps(steps, &format!("{path}.steps"), names, nested),
                }
            }
            StepKind::Par { branches } | StepKind::Race { branches } => {
                self.nonempty(path, "branches", branches);
                let before = names.clone();
                let mut outcomes = Vec::new();
                let mut nodes = Vec::new();
                for (index, branch) in branches.iter().enumerate() {
                    let mut local = before.clone();
                    nodes.push(self.step(
                        branch,
                        format!("{path}.branches[{index}]"),
                        &mut local,
                        nested,
                    ));
                    outcomes.push(local);
                }
                if matches!(kind, StepKind::Par { .. }) {
                    let mut seen: BTreeSet<&String> = BTreeSet::new();
                    for outcome in &outcomes {
                        for name in outcome.bound.difference(&before.bound) {
                            if !seen.insert(name) {
                                self.problem(
                                    path,
                                    None,
                                    format!(
                                        "two parallel branches both bind `{name}`; give each branch its own name"
                                    ),
                                );
                            }
                        }
                    }
                    for outcome in outcomes {
                        names.bound.extend(outcome.bound);
                        names.receipts.extend(outcome.receipts);
                    }
                    Op::Par { branches: nodes }
                } else {
                    *names = Names::certain(&outcomes);
                    Op::Race { branches: nodes }
                }
            }
            StepKind::FanOut { over, each, steps } => {
                self.nonempty(path, "steps", steps);
                for item in over {
                    self.template(path, "over", item, names, &[]);
                }
                let mut local = names.clone();
                local.bound.insert(each.clone());
                let body = self.steps(steps, &format!("{path}.steps"), &mut local, nested);
                Op::FanOut {
                    over: over.clone(),
                    each: each.clone(),
                    body,
                }
            }
            StepKind::Loop {
                max_iterations,
                until,
                steps,
            } => {
                self.nonempty(path, "steps", steps);
                let body = self.steps(steps, &format!("{path}.steps"), names, nested);
                if let Some(check) = until {
                    self.check(&format!("{path}.until"), check, names, &[]);
                }
                Op::Loop {
                    max_iterations: *max_iterations,
                    until: until.clone(),
                    body,
                }
            }
            StepKind::When {
                check,
                steps,
                otherwise,
            } => {
                self.check(&format!("{path}.check"), check, names, &[]);
                let mut then_names = names.clone();
                let mut else_names = names.clone();
                let then = self.steps(steps, &format!("{path}.steps"), &mut then_names, nested);
                let otherwise_nodes = self.steps(
                    otherwise,
                    &format!("{path}.otherwise"),
                    &mut else_names,
                    nested,
                );
                *names = Names::certain(&[then_names, else_names]);
                Op::When {
                    check: check.clone(),
                    then,
                    otherwise: otherwise_nodes,
                }
            }
            StepKind::Guard {
                phase,
                check,
                feedback,
                max_attempts,
                on_exhausted,
                steps,
            } => {
                self.nonempty(path, "steps", steps);
                let mut body_names = names.clone();
                body_names.bound.insert("feedback".to_owned());
                let body = self.steps(steps, &format!("{path}.steps"), &mut body_names, nested);
                body_names.bound.remove("feedback");
                match phase {
                    super::model::GuardPhase::Pre => {
                        self.check(&format!("{path}.check"), check, names, &[]);
                    }
                    super::model::GuardPhase::Post => {
                        self.check(&format!("{path}.check"), check, &body_names, &["output"]);
                    }
                }
                *names = body_names;
                Op::Guard {
                    phase: *phase,
                    check: check.clone(),
                    feedback: feedback.clone(),
                    max_attempts: *max_attempts,
                    on_exhausted: *on_exhausted,
                    body,
                }
            }
            StepKind::Budget { tokens, steps } => {
                self.nonempty(path, "steps", steps);
                Op::Budget {
                    tokens: *tokens,
                    body: self.steps(steps, &format!("{path}.steps"), names, nested),
                }
            }
            StepKind::Within { world, steps } => {
                self.nonempty(path, "steps", steps);
                if !self.program.worlds.contains_key(world) {
                    self.problem(
                        path,
                        Some(("world", world)),
                        format!(
                            "unknown world `{world}`; declared worlds: {}",
                            list(self.program.worlds.keys())
                        ),
                    );
                }
                Op::Within {
                    world: world.clone(),
                    body: self.steps(steps, &format!("{path}.steps"), names, nested),
                }
            }
            StepKind::Retry { steps } => {
                self.nonempty(path, "steps", steps);
                Op::Retry {
                    body: self.steps(steps, &format!("{path}.steps"), names, nested),
                }
            }
            StepKind::Recover { steps, fallback } => {
                self.nonempty(path, "steps", steps);
                self.nonempty(path, "fallback", fallback);
                let mut body_names = names.clone();
                let mut fallback_names = names.clone();
                let body = self.steps(steps, &format!("{path}.steps"), &mut body_names, nested);
                let recovered = self.steps(
                    fallback,
                    &format!("{path}.fallback"),
                    &mut fallback_names,
                    nested,
                );
                *names = Names::certain(&[body_names, fallback_names]);
                Op::Recover {
                    body,
                    fallback: recovered,
                }
            }
            StepKind::Emit { topic, payload } => {
                self.template(path, "payload", payload, names, &[]);
                Op::Emit {
                    topic: topic.clone(),
                    payload: payload.clone(),
                }
            }
            StepKind::AwaitSignal { topic } => Op::AwaitSignal {
                topic: topic.clone(),
            },
            StepKind::Interrupt { prompt } => {
                self.template(path, "prompt", prompt, names, &[]);
                Op::Interrupt {
                    prompt: prompt.clone(),
                }
            }
            StepKind::Delay { duration } => {
                let ms = match duration.to_duration() {
                    Ok(duration) => i64::try_from(duration.as_millis()).unwrap_or(i64::MAX),
                    Err(error) => {
                        self.problem(path, None, format!("duration: {error}"));
                        0
                    }
                };
                Op::Delay { ms }
            }
            StepKind::Pin { fact } => Op::Pin { fact: fact.clone() },
            StepKind::Checkpoint { name } => Op::Checkpoint { name: name.clone() },
        }
    }

    fn nonempty(&mut self, path: &str, field: &str, steps: &[Step]) {
        if steps.is_empty() {
            self.problem(
                path,
                None,
                format!("`{field}` is empty; it needs at least one step"),
            );
        }
    }
}

fn collect_strings<'a>(value: &'a serde_json::Value, out: &mut Vec<&'a str>) {
    match value {
        serde_json::Value::String(text) => out.push(text),
        serde_json::Value::Array(items) => items.iter().for_each(|item| collect_strings(item, out)),
        serde_json::Value::Object(map) => map.values().for_each(|item| collect_strings(item, out)),
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

fn list<'a>(names: impl IntoIterator<Item = impl AsRef<str> + 'a>) -> String {
    let names: Vec<String> = names
        .into_iter()
        .map(|name| name.as_ref().to_owned())
        .collect();
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
    }
}
