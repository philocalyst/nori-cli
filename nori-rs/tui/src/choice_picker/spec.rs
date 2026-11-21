//! A choice list described as data: what it shows, and what choosing, asking, keying and being
//! told something does. Actions answer with a [`Next`], so a picker can lead to another list
//! (`Show`), to a question (`Ask`, and a question can ask the next one), or end.

use std::any::Any;
use std::sync::Arc;

use crate::app_event_sender::AppEventSender;
use crate::bottom_pane::SelectionAction;

/// What follows an action.
pub(crate) enum Next {
    /// Close the picker.
    Done,
    /// Replace the list with this one.
    Show(ChoiceSpec),
    /// Ask for a line of text.
    Ask(Ask),
}

/// A row action that leads somewhere.
pub(crate) type StepAction = Arc<dyn Fn(&AppEventSender) -> Next + Send + Sync>;
/// Receives the text a question collected: the next step, or why the text will not do (shown,
/// and the question stays).
pub(crate) type AnswerAction =
    Arc<dyn Fn(&str, &AppEventSender) -> Result<Next, String> + Send + Sync>;
/// Told something the app learned while the picker is open (an agent's auth status arrived);
/// answers with a new list or nothing. The notice is whatever the app passes to
/// `ChoiceOverlay::notify`; the action downcasts what it understands.
pub(crate) type NoticeAction = Arc<dyn Fn(&dyn Any, &AppEventSender) -> Option<Next> + Send + Sync>;

/// A question in the prompt row.
pub(crate) struct Ask {
    /// The verb beside the prompt: `iterations ❯ …`.
    pub(crate) verb: &'static str,
    /// What the prompt names beside the verb; the selected row's label when empty.
    pub(crate) label: String,
    /// The text the prompt starts with.
    pub(crate) initial: String,
    /// The answer is a credential: typed as dots.
    pub(crate) secret: bool,
    pub(crate) then: AnswerAction,
}

/// What choosing an item does.
pub(crate) enum ChoiceAction {
    /// Run this, then close.
    Run(SelectionAction),
    /// Lead somewhere.
    Step(StepAction),
    /// Ask for a line of text first.
    Ask(Ask),
}

pub(crate) struct ChoiceItem {
    pub(crate) label: String,
    pub(crate) detail: Option<String>,
    /// Other words a query finds this item by (its agent, say).
    pub(crate) keywords: Option<String>,
    pub(crate) section: Option<String>,
    /// The value that is in effect now.
    pub(crate) current: bool,
    /// Listed with its `detail` as the reason, but not choosable.
    pub(crate) disabled: bool,
    /// Choosing runs the action and leaves the picker open.
    pub(crate) keep_open: bool,
    pub(crate) action: ChoiceAction,
    /// Row actions by name; the spec's `keys` bind them to keys.
    pub(crate) keys: Vec<(&'static str, StepAction)>,
}

impl ChoiceItem {
    pub(crate) fn run(label: impl Into<String>, action: SelectionAction) -> Self {
        Self::new(label, ChoiceAction::Run(action))
    }

    pub(crate) fn step(label: impl Into<String>, action: StepAction) -> Self {
        Self::new(label, ChoiceAction::Step(action))
    }

    pub(crate) fn ask(label: impl Into<String>, ask: Ask) -> Self {
        Self::new(label, ChoiceAction::Ask(ask))
    }

    fn new(label: impl Into<String>, action: ChoiceAction) -> Self {
        Self {
            label: label.into(),
            detail: None,
            keywords: None,
            section: None,
            current: false,
            disabled: false,
            keep_open: false,
            action,
            keys: Vec::new(),
        }
    }

    pub(crate) fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub(crate) fn section(mut self, section: impl Into<String>) -> Self {
        self.section = Some(section.into());
        self
    }

    pub(crate) fn current(mut self, current: bool) -> Self {
        self.current = current;
        self
    }

    pub(crate) fn key(mut self, name: &'static str, action: StepAction) -> Self {
        self.keys.push((name, action));
        self
    }
}

/// Which of several toggled items to act on.
pub(crate) type MultiAction = Box<dyn Fn(&[usize], &AppEventSender) + Send + Sync>;

pub(crate) struct ChoiceSpec {
    pub(crate) title: String,
    /// One line of context shown on the bottom edge.
    pub(crate) subtitle: Option<String>,
    pub(crate) items: Vec<ChoiceItem>,
    /// Makes the picker multi-select: `Space` toggles rows and Enter calls this with the
    /// indexes (into `items`) toggled. Each item's own action is not used.
    pub(crate) multi: Option<MultiAction>,
    /// The item highlighted when the picker opens.
    pub(crate) initial: Option<usize>,
    /// Typing filters the list. Otherwise plain keys are the row actions in `keys`.
    pub(crate) searchable: bool,
    /// Shown in the prompt row of a picker that is not searchable.
    pub(crate) hint: String,
    /// `(key, action name)` pairs for a picker that is not searchable.
    pub(crate) keys: &'static [(char, &'static str)],
    /// Esc goes here instead of closing.
    pub(crate) on_back: Option<StepAction>,
    /// Shift-Tab: a secondary action that leaves the picker open.
    pub(crate) on_secondary: Option<SelectionAction>,
    pub(crate) on_notice: Option<NoticeAction>,
    /// Run when the picker is closed without choosing.
    pub(crate) on_dismiss: Option<SelectionAction>,
}

impl std::fmt::Debug for ChoiceSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChoiceSpec")
            .field("title", &self.title)
            .field("items", &self.items.len())
            .finish()
    }
}

impl ChoiceSpec {
    pub(crate) fn new(title: impl Into<String>, items: Vec<ChoiceItem>) -> Self {
        Self {
            title: title.into(),
            subtitle: None,
            items,
            multi: None,
            initial: None,
            searchable: true,
            hint: String::new(),
            keys: &[],
            on_back: None,
            on_secondary: None,
            on_notice: None,
            on_dismiss: None,
        }
    }
}
