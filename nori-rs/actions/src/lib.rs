//! Nori's actions: every user-facing command, defined once.
//!
//! An action is something the user can make Nori do: switch the model, start a new session, move
//! the session to another machine, attach a file, open the usage view. Each is one [Action] in a
//! [Registry], and every way of reaching it reads from there:
//!
//! - the **palette** (`Alt+X` or Helix's `:` anywhere, even mid-prompt: the draft is never
//!   touched), which searches titles, names and keywords;
//! - **typed commands** (`/model opus` as the whole draft), resolved by name or alias;
//! - **keys**: an action's bindings are part of its definition;
//! - **directives**: an action whose [Effect] is [Effect::OnSend] can be put into the draft as a
//!   chip (`⚙ model: opus`) and applies when that prompt is sent, so changing things mid-prompt
//!   never disturbs what is being written;
//! - **plugins**: the Pkl runtime registers actions of its own ([Source::Plugin]); the TUI runs
//!   them by handing their [Invocation] back.
//!
//! Whether an action can run now is a property of the action ([Requires]) checked against the
//! [Situation], so the palette can show unavailable actions dimmed with the reason instead of
//! hiding them.

mod registry;
mod search;

pub use registry::Registry;
pub use search::Hit;

use serde::Deserialize;
use serde::Serialize;

/// Identifies an action, dotted and stable (`model.set`, `session.new`): what keys are bound to
/// in config and what plugins name.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct ActionId(String);

impl ActionId {
    pub fn new(id: impl Into<String>) -> Self {
        ActionId(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ActionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What part of Nori an action is about, for grouping in the palette.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub enum Category {
    Conversation,
    Session,
    Model,
    Agent,
    Machine,
    Context,
    View,
    Settings,
    Account,
    Tools,
    Plugin,
}

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Category::Conversation => "conversation",
            Category::Session => "session",
            Category::Model => "model",
            Category::Agent => "agent",
            Category::Machine => "machine",
            Category::Context => "context",
            Category::View => "view",
            Category::Settings => "settings",
            Category::Account => "account",
            Category::Tools => "tools",
            Category::Plugin => "plugin",
        }
    }
}

/// When an action takes effect.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Effect {
    /// When it is chosen.
    Now,
    /// When the prompt it is attached to is sent. Chosen from the palette with a draft open, it
    /// becomes a chip in the draft; chosen on an empty draft, it applies at once.
    OnSend,
}

/// Where the choices of a [Param::Choice] come from: the TUI fills them in.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ChoiceSource {
    Models,
    Efforts,
    Modes,
    Agents,
    Machines,
    Skills,
    Files,
    Sessions,
    Fixed(Vec<Choice>),
}

/// One value an action may take.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Choice {
    pub value: String,
    pub label: String,
    pub detail: Option<String>,
}

/// What an action takes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Param {
    None,
    /// One of a set. Without a value the TUI opens the choices.
    Choice(ChoiceSource),
    /// Free text, optional unless `required`.
    Text {
        placeholder: String,
        required: bool,
    },
}

/// What an action needs to run.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Requires {
    /// No turn may be running (it replaces or rewrites the conversation).
    pub idle: bool,
    /// Only in the main conversation, not a subagent's view.
    pub main: bool,
    /// Only for a session on this machine (not a cloud session).
    pub local: bool,
    /// Only for a cloud session.
    pub cloud: bool,
}

/// What is going on, for deciding which actions can run.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Situation {
    pub turn_running: bool,
    pub in_child: bool,
    pub cloud: bool,
}

impl Requires {
    /// `Err` with the reason when the action cannot run in `situation`.
    pub fn check(&self, situation: &Situation) -> Result<(), &'static str> {
        if self.idle && situation.turn_running {
            return Err("waits for the running turn to finish");
        }
        if self.main && situation.in_child {
            return Err("runs in the main conversation");
        }
        if self.local && situation.cloud {
            return Err("only for a session on this machine");
        }
        if self.cloud && !situation.cloud {
            return Err("only for a cloud session");
        }
        Ok(())
    }
}

/// Who defined an action.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Source {
    /// Nori itself.
    BuiltIn,
    /// A command the agent advertised (`/claude-code:loop`).
    Agent,
    /// A custom prompt file.
    Prompt,
    /// A plugin of the Pkl runtime, by name.
    Plugin(String),
}

/// One action.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Action {
    pub id: ActionId,
    /// What the palette shows: "Switch model".
    pub title: String,
    /// One line on what it does.
    pub summary: String,
    /// What a user types after `/`: the first is the canonical name.
    pub names: Vec<String>,
    /// More words it is found by.
    pub keywords: Vec<String>,
    pub category: Category,
    pub param: Param,
    pub effect: Effect,
    pub requires: Requires,
    /// Its key bindings, as shown (`ctrl+n`).
    pub keys: Vec<String>,
    pub source: Source,
}

impl Action {
    /// An action that runs when chosen and takes nothing.
    pub fn new(id: &str, title: &str, summary: &str, category: Category) -> Self {
        Action {
            id: ActionId::new(id),
            title: title.to_owned(),
            summary: summary.to_owned(),
            names: Vec::new(),
            keywords: Vec::new(),
            category,
            param: Param::None,
            effect: Effect::Now,
            requires: Requires::default(),
            keys: Vec::new(),
            source: Source::BuiltIn,
        }
    }

    pub fn named(mut self, names: &[&str]) -> Self {
        self.names = names.iter().map(|name| (*name).to_owned()).collect();
        self
    }

    pub fn keywords(mut self, keywords: &[&str]) -> Self {
        self.keywords = keywords.iter().map(|word| (*word).to_owned()).collect();
        self
    }

    pub fn param(mut self, param: Param) -> Self {
        self.param = param;
        self
    }

    pub fn on_send(mut self) -> Self {
        self.effect = Effect::OnSend;
        self
    }

    pub fn requires(mut self, requires: Requires) -> Self {
        self.requires = requires;
        self
    }

    pub fn keys(mut self, keys: &[&str]) -> Self {
        self.keys = keys.iter().map(|key| (*key).to_owned()).collect();
        self
    }

    pub fn source(mut self, source: Source) -> Self {
        self.source = source;
        self
    }

    /// The name typed after `/`, if it has one.
    pub fn name(&self) -> Option<&str> {
        self.names.first().map(String::as_str)
    }
}

/// An action chosen, with its argument.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Invocation {
    pub id: ActionId,
    pub arg: Option<String>,
}

impl Invocation {
    pub fn new(id: &ActionId, arg: Option<String>) -> Self {
        Invocation {
            id: id.clone(),
            arg,
        }
    }
}
