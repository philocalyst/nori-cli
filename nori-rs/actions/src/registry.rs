//! The registry: every action by id, in the order they were registered.

use std::collections::HashMap;

use crate::Action;
use crate::ActionId;
use crate::Invocation;
use crate::Param;
use crate::Source;

#[derive(Clone, Debug, Default)]
pub struct Registry {
    actions: Vec<Action>,
    by_id: HashMap<ActionId, usize>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add an action, or replace the one with its id (a plugin may override a built-in).
    pub fn register(&mut self, action: Action) {
        match self.by_id.get(&action.id) {
            Some(&index) => self.actions[index] = action,
            None => {
                self.by_id.insert(action.id.clone(), self.actions.len());
                self.actions.push(action);
            }
        }
    }

    /// Drop every action from `source` (an agent's commands when it changes, a plugin's when it
    /// unloads).
    pub fn remove_source(&mut self, source: &Source) {
        self.actions.retain(|action| &action.source != source);
        self.reindex();
    }

    fn reindex(&mut self) {
        self.by_id = self
            .actions
            .iter()
            .enumerate()
            .map(|(index, action)| (action.id.clone(), index))
            .collect();
    }

    pub fn get(&self, id: &ActionId) -> Option<&Action> {
        self.by_id.get(id).map(|&index| &self.actions[index])
    }

    pub fn actions(&self) -> &[Action] {
        &self.actions
    }

    /// The action typed as `/name` (by any of its names, ignoring case).
    pub fn by_name(&self, name: &str) -> Option<&Action> {
        self.actions.iter().find(|action| {
            action
                .names
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(name))
        })
    }

    /// The invocation a typed command line means: `/name` or `/name argument`.
    pub fn parse_command(&self, line: &str) -> Option<Invocation> {
        let rest = line.trim_start().strip_prefix('/')?;
        let (name, arg) = match rest.split_once(char::is_whitespace) {
            Some((name, arg)) => (name, arg.trim()),
            None => (rest, ""),
        };
        let action = self.by_name(name)?;
        let arg = match (&action.param, arg.is_empty()) {
            (_, true) => None,
            (Param::None, false) => return None,
            (Param::Choice(_) | Param::Text { .. }, false) => Some(arg.to_owned()),
        };
        Some(Invocation::new(&action.id, arg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Category;
    use crate::ChoiceSource;
    use pretty_assertions::assert_eq;

    fn registry() -> Registry {
        let mut registry = Registry::new();
        registry.register(
            Action::new(
                "model.set",
                "Switch model",
                "choose a model",
                Category::Model,
            )
            .named(&["model", "m"])
            .param(Param::Choice(ChoiceSource::Models))
            .on_send(),
        );
        registry.register(
            Action::new(
                "session.new",
                "New session",
                "start over",
                Category::Session,
            )
            .named(&["new"]),
        );
        registry
    }

    #[test]
    fn typed_commands_resolve_by_any_name() {
        let registry = registry();
        assert_eq!(
            registry.parse_command("/m opus 4"),
            Some(Invocation::new(
                &ActionId::new("model.set"),
                Some("opus 4".into())
            ))
        );
        assert_eq!(
            registry.parse_command("/NEW"),
            Some(Invocation::new(&ActionId::new("session.new"), None))
        );
        // `/new` takes nothing: with an argument it is not that command.
        assert_eq!(registry.parse_command("/new please"), None);
        assert_eq!(registry.parse_command("/unknown"), None);
    }

    #[test]
    fn registering_an_id_again_replaces_it() {
        let mut registry = registry();
        registry.register(
            Action::new("session.new", "Fresh start", "plugin's", Category::Session)
                .source(Source::Plugin("mine".into())),
        );
        assert_eq!(registry.actions().len(), 2);
        assert_eq!(
            registry
                .get(&ActionId::new("session.new"))
                .map(|action| action.title.as_str()),
            Some("Fresh start")
        );
        registry.remove_source(&Source::Plugin("mine".into()));
        assert_eq!(registry.actions().len(), 1);
        assert!(registry.get(&ActionId::new("model.set")).is_some());
    }
}
