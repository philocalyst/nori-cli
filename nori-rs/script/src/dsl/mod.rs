//! The `nori/workflow` builtin module: the DSL's vocabulary.
//!
//! Every clause takes a workflow first and returns a new one, so clauses chain
//! with Steel's thread-first macro, `~>`, and inside `define-workflow`.

mod args;

use std::fmt;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use steel::SteelVal;
use steel::rvals::Custom;
use steel::rvals::Result;
use steel::steel_vm::builtin::BuiltInModule;

use crate::model::CommitAgent;
use crate::model::CommitBatch;
use crate::model::CommitRules;
use crate::model::CommitStyle;
use crate::model::Hook;
use crate::model::HookEvent;
use crate::model::PrRules;
use crate::model::Route;
use crate::model::Upstream;
use crate::model::Workflow;

pub(crate) const MODULE_NAME: &str = "nori/workflow";

/// Scheme run once per engine, before any script: loads the module and
/// defines the `define-workflow` macro.
pub(crate) const PRELUDE: &str = r#"
(require-builtin nori/workflow)

(define-syntax define-workflow
  (syntax-rules ()
    [(define-workflow name clause ...)
     (define name (register-workflow (~> (workflow (quote name)) clause ...)))]))
"#;

/// Long hook names matching the `.hook_after_change(prompt)` phrasing.
const HOOK_ALIASES: [(HookEvent, &str); 7] = [
    (HookEvent::AfterChange, "hook-after-change"),
    (HookEvent::AfterTurn, "hook-after-turn"),
    (HookEvent::AfterCommit, "hook-after-commit"),
    (HookEvent::BeforePr, "hook-before-pr"),
    (HookEvent::BeforeStop, "hook-before-stop"),
    (HookEvent::CiFailed, "hook-after-ci-fail"),
    (HookEvent::CiPassed, "hook-after-ci-pass"),
];

/// A workflow as a Steel value. Values are immutable: clauses return copies.
#[derive(Clone, Debug)]
pub(crate) struct WorkflowValue(pub Workflow);

impl Custom for WorkflowValue {
    fn fmt(&self) -> Option<std::result::Result<String, fmt::Error>> {
        Some(Ok(format!("#<workflow {}>", self.0.name)))
    }
}

/// Workflows registered while scripts run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Registry {
    /// In first-registration order; re-registering a name replaces it in place.
    pub workflows: Vec<Workflow>,
    pub default: Option<String>,
}

impl Registry {
    fn register(&mut self, workflow: Workflow) {
        match self
            .workflows
            .iter_mut()
            .find(|existing| existing.name == workflow.name)
        {
            Some(existing) => *existing = workflow,
            None => self.workflows.push(workflow),
        }
    }

    pub(crate) fn get(&self, name: &str) -> Option<&Workflow> {
        self.workflows.iter().find(|workflow| workflow.name == name)
    }
}

pub(crate) type SharedRegistry = Arc<Mutex<Registry>>;

fn with_registry<T>(registry: &SharedRegistry, f: impl FnOnce(&mut Registry) -> T) -> T {
    f(&mut registry.lock().unwrap_or_else(PoisonError::into_inner))
}

pub(crate) fn snapshot(registry: &SharedRegistry) -> Registry {
    with_registry(registry, |registry| registry.clone())
}

pub(crate) fn replace(registry: &SharedRegistry, snapshot: Registry) {
    with_registry(registry, |registry| *registry = snapshot);
}

/// The workflow `value` holds, if it holds one.
pub(crate) fn workflow_of(value: &SteelVal) -> Option<Workflow> {
    args::workflow("", value).ok()
}

pub(crate) fn value_of(workflow: Workflow) -> Result<SteelVal> {
    args::into_value(workflow)
}

/// Builds `nori/workflow` around `registry`, which `register-workflow` and
/// `default-workflow` write to.
pub(crate) fn module(registry: &SharedRegistry) -> BuiltInModule {
    let mut module = BuiltInModule::new(MODULE_NAME);
    module
        .register_value("workflow", SteelVal::FuncV(workflow))
        .register_value("workflow?", SteelVal::FuncV(is_workflow))
        .register_value("turns", SteelVal::FuncV(turns))
        .register_value("instruct", SteelVal::FuncV(instruct))
        .register_value("commit-style", SteelVal::FuncV(commit_style))
        .register_value("commit-agent", SteelVal::FuncV(commit_agent))
        .register_value("commit-review", SteelVal::FuncV(commit_review))
        .register_value("fallback-models", SteelVal::FuncV(fallback_models))
        .register_value(
            "fallback-ranked-models",
            SteelVal::FuncV(fallback_ranked_models),
        )
        .register_value("pr", SteelVal::FuncV(pr));
    for event in HookEvent::ALL {
        module.register_value(event.form(), hook(event));
    }
    for (event, alias) in HOOK_ALIASES {
        module.register_value(alias, hook(event));
    }
    let lookup = Arc::clone(registry);
    module.register_value(
        "use",
        boxed(move |args| {
            let [current, base] = args::exact::<2>("use", "(use base)", args)?;
            let current = args::workflow("use", current)?;
            let base = match base {
                SteelVal::StringV(name) | SteelVal::SymbolV(name) => {
                    let name = name.as_str();
                    with_registry(&lookup, |registry| registry.get(name).cloned()).ok_or_else(
                        || {
                            args::error(
                                "use",
                                format_args!("no workflow named {name:?} is defined yet"),
                            )
                        },
                    )?
                }
                other => args::workflow("use", other)?,
            };
            args::into_value(Workflow {
                name: current.name,
                ..base
            })
        }),
    );
    let registrations = Arc::clone(registry);
    module.register_value(
        "register-workflow",
        boxed(move |args| {
            let [value] =
                args::exact::<1>("register-workflow", "(register-workflow workflow)", args)?;
            let workflow = args::workflow("register-workflow", value)?;
            with_registry(&registrations, |registry| registry.register(workflow));
            Ok(value.clone())
        }),
    );
    let defaults = Arc::clone(registry);
    module.register_value(
        "default-workflow",
        boxed(move |args| {
            let [value] =
                args::exact::<1>("default-workflow", "(default-workflow workflow)", args)?;
            let name = match value {
                SteelVal::StringV(name) | SteelVal::SymbolV(name) => {
                    let name = name.as_str().to_string();
                    if with_registry(&defaults, |registry| registry.get(&name).is_none()) {
                        return Err(args::error(
                            "default-workflow",
                            format_args!("no workflow named {name:?} is defined yet"),
                        ));
                    }
                    name
                }
                other => {
                    let workflow = args::workflow("default-workflow", other)?;
                    let name = workflow.name.clone();
                    with_registry(&defaults, |registry| registry.register(workflow));
                    name
                }
            };
            with_registry(&defaults, |registry| registry.default = Some(name));
            Ok(value.clone())
        }),
    );
    module
}

fn boxed(function: impl Fn(&[SteelVal]) -> Result<SteelVal> + Send + Sync + 'static) -> SteelVal {
    SteelVal::anonymous_boxed_function(Arc::new(function))
}

fn workflow(args: &[SteelVal]) -> Result<SteelVal> {
    let [name] = args::exact::<1>("workflow", "(workflow \"name\")", args)?;
    args::into_value(Workflow::named(args::name("workflow", "the name", name)?))
}

fn is_workflow(args: &[SteelVal]) -> Result<SteelVal> {
    let [value] = args::exact::<1>("workflow?", "(workflow? value)", args)?;
    Ok(SteelVal::BoolV(args::workflow("workflow?", value).is_ok()))
}

/// `(fallback-models "provider/model" ...)` replaces the ordered fallback list.
fn fallback_models(args: &[SteelVal]) -> Result<SteelVal> {
    const FORM: &str = "fallback-models";
    let Some((workflow, models)) = args.split_first() else {
        return Err(args::arity(
            FORM,
            "(fallback-models \"provider/model\" ...)",
        ));
    };
    let mut workflow = args::workflow(FORM, workflow)?;
    let mut selected = Vec::new();
    for model in models {
        let id = args::text(FORM, "a model ID", model)?;
        if id.chars().any(char::is_whitespace) {
            return Err(args::error(
                FORM,
                format_args!("model IDs must not contain whitespace, got {id:?}"),
            ));
        }
        if selected.contains(&id) {
            return Err(args::error(
                FORM,
                format_args!("model ID {id:?} is given twice"),
            ));
        }
        selected.push(id);
    }
    let fallback = &mut workflow.recovery.base.fallback;
    fallback.chain = selected.iter().map(|id| Route::parse_legacy(id)).collect();
    fallback.ranked = false;
    args::into_value(workflow)
}

/// Use the ranked provider catalog instead of an explicit list.
fn fallback_ranked_models(args: &[SteelVal]) -> Result<SteelVal> {
    const FORM: &str = "fallback-ranked-models";
    let [workflow] = args::exact::<1>(FORM, "(fallback-ranked-models)", args)?;
    let mut workflow = args::workflow(FORM, workflow)?;
    let fallback = &mut workflow.recovery.base.fallback;
    fallback.chain.clear();
    fallback.ranked = true;
    args::into_value(workflow)
}

fn turns(args: &[SteelVal]) -> Result<SteelVal> {
    let [workflow, turns] = args::exact::<2>("turns", "(turns n)", args)?;
    let mut workflow = args::workflow("turns", workflow)?;
    workflow.max_turns = Some(args::integer("turns", "the turn count", 0, turns)?);
    args::into_value(workflow)
}

fn hook(event: HookEvent) -> SteelVal {
    boxed(move |args| {
        let form = event.form();
        let usage = format!("({form} \"prompt\")");
        let [workflow, prompt] = args::exact::<2>(form, &usage, args)?;
        let mut workflow = args::workflow(form, workflow)?;
        workflow.hooks.push(Hook {
            event,
            prompt: args::text(form, "the prompt", prompt)?,
        });
        args::into_value(workflow)
    })
}

fn instruct(args: &[SteelVal]) -> Result<SteelVal> {
    let [workflow, text] = args::exact::<2>("instruct", "(instruct \"text\")", args)?;
    let mut workflow = args::workflow("instruct", workflow)?;
    workflow
        .instructions
        .push(args::text("instruct", "the instruction", text)?);
    args::into_value(workflow)
}

const COMMIT_STYLES: [(&str, CommitStyle); 2] = [
    ("conventional", CommitStyle::Conventional),
    ("plain", CommitStyle::Plain),
];

const COMMIT_BATCHES: [(&str, CommitBatch); 3] = [
    ("per-change", CommitBatch::PerChange),
    ("per-turn", CommitBatch::PerTurn),
    ("per-task", CommitBatch::PerTask),
];

const UPSTREAM: [(&str, Upstream); 4] = [
    ("never", Upstream::Never),
    ("ask", Upstream::Ask),
    ("tests-pass", Upstream::TestsPass),
    ("done", Upstream::Done),
];

#[derive(Clone, Copy)]
enum CommitOption {
    MaxFiles,
    MaxSubject,
    Atomic,
    Batch,
    RequireBody,
}

const COMMIT_OPTIONS: [(&str, CommitOption); 5] = [
    ("#:max-files", CommitOption::MaxFiles),
    ("#:max-subject", CommitOption::MaxSubject),
    ("#:atomic", CommitOption::Atomic),
    ("#:batch", CommitOption::Batch),
    ("#:require-body", CommitOption::RequireBody),
];

#[derive(Clone, Copy)]
enum PrOption {
    Title,
    Sections,
    MaxLines,
    MaxFiles,
    Base,
    Draft,
    UpstreamWhen,
}

const PR_OPTIONS: [(&str, PrOption); 7] = [
    ("#:title", PrOption::Title),
    ("#:sections", PrOption::Sections),
    ("#:max-lines", PrOption::MaxLines),
    ("#:max-files", PrOption::MaxFiles),
    ("#:base", PrOption::Base),
    ("#:draft", PrOption::Draft),
    ("#:upstream-when", PrOption::UpstreamWhen),
];

/// `(commit-style 'conventional #:max-files 8 ...)`; the style may be left
/// out to adjust options only. Options not given keep their current value.
fn commit_style(args: &[SteelVal]) -> Result<SteelVal> {
    const FORM: &str = "commit-style";
    let Some((workflow, rest)) = args.split_first() else {
        return Err(args::arity(
            FORM,
            "(commit-style 'conventional #:max-files 8)",
        ));
    };
    let mut workflow = args::workflow(FORM, workflow)?;
    let mut rules = workflow.commit.take().unwrap_or_default();
    let options = match rest.split_first() {
        Some((style, options)) if !matches!(style, SteelVal::SymbolV(s) if s.starts_with("#:")) => {
            rules.style = Some(args::choice(FORM, "the style", &COMMIT_STYLES, style)?);
            options
        }
        _ => rest,
    };
    for (keyword, option, value) in args::keywords(FORM, &COMMIT_OPTIONS, options)? {
        match option {
            CommitOption::MaxFiles => {
                rules.max_files = Some(args::integer(FORM, keyword, 1, value)?)
            }
            CommitOption::MaxSubject => {
                rules.max_subject = Some(args::integer(FORM, keyword, 20, value)?)
            }
            CommitOption::Atomic => rules.atomic = args::boolean(FORM, keyword, value)?,
            CommitOption::Batch => {
                rules.batch = Some(args::choice(FORM, keyword, &COMMIT_BATCHES, value)?)
            }
            CommitOption::RequireBody => rules.require_body = args::boolean(FORM, keyword, value)?,
        }
    }
    workflow.commit = Some(rules);
    args::into_value(workflow)
}

/// `(commit-agent 'claude #:model "haiku")`: the agent that writes commits.
fn commit_agent(args: &[SteelVal]) -> Result<SteelVal> {
    const FORM: &str = "commit-agent";
    const USAGE: &str = "(commit-agent 'claude #:model \"haiku\")";
    let [workflow, agent, options @ ..] = args else {
        return Err(args::arity(FORM, USAGE));
    };
    let mut workflow = args::workflow(FORM, workflow)?;
    let mut designated = CommitAgent {
        agent: args::name(FORM, "the agent", agent)?,
        model: None,
    };
    for (keyword, (), value) in args::keywords(FORM, &[("#:model", ())], options)? {
        designated.model = Some(args::text(FORM, keyword, value)?);
    }
    let mut rules: CommitRules = workflow.commit.take().unwrap_or_default();
    rules.agent = Some(designated);
    workflow.commit = Some(rules);
    args::into_value(workflow)
}

/// `(commit-review 'claude #:model "haiku")` designates a review-only child.
fn commit_review(args: &[SteelVal]) -> Result<SteelVal> {
    const FORM: &str = "commit-review";
    let [workflow, agent, options @ ..] = args else {
        return Err(args::arity(
            FORM,
            "(commit-review 'claude #:model \"haiku\")",
        ));
    };
    let mut workflow = args::workflow(FORM, workflow)?;
    let mut reviewer = CommitAgent {
        agent: args::name(FORM, "the agent", agent)?,
        model: None,
    };
    for (keyword, (), value) in args::keywords(FORM, &[("#:model", ())], options)? {
        reviewer.model = Some(args::text(FORM, keyword, value)?);
    }
    workflow.commit_reviewer = Some(reviewer);
    args::into_value(workflow)
}

/// `(pr #:title "..." #:sections '("Why") ...)`. Options not given keep their
/// current value.
fn pr(args: &[SteelVal]) -> Result<SteelVal> {
    const FORM: &str = "pr";
    let Some((workflow, options)) = args.split_first() else {
        return Err(args::arity(FORM, "(pr #:max-lines 400)"));
    };
    let mut workflow = args::workflow(FORM, workflow)?;
    let mut rules: PrRules = workflow.pr.take().unwrap_or_default();
    for (keyword, option, value) in args::keywords(FORM, &PR_OPTIONS, options)? {
        match option {
            PrOption::Title => rules.title = Some(args::text(FORM, keyword, value)?),
            PrOption::Sections => rules.sections = args::texts(FORM, keyword, value)?,
            PrOption::MaxLines => rules.max_lines = Some(args::integer(FORM, keyword, 1, value)?),
            PrOption::MaxFiles => rules.max_files = Some(args::integer(FORM, keyword, 1, value)?),
            PrOption::Base => rules.base = Some(args::text(FORM, keyword, value)?),
            PrOption::Draft => rules.draft = args::boolean(FORM, keyword, value)?,
            PrOption::UpstreamWhen => {
                rules.upstream = Some(args::choice(FORM, keyword, &UPSTREAM, value)?)
            }
        }
    }
    workflow.pr = Some(rules);
    args::into_value(workflow)
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
