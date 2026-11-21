//! Control steps: branching on a value, binding a variable, announcing slash commands.

use regex::Regex;
use serde_json::Value;

use super::Break;
use super::Ctx;
use super::Run;
use crate::agent::available_commands;
use crate::world::CommandsStep;
use crate::world::SetStep;
use crate::world::WhenStep;

impl Ctx {
    pub(super) async fn when(&self, when: &WhenStep) -> Run {
        let holds = match (&when.matches, &when.equals, &when.bound) {
            (Some(pattern), None, None) => match Regex::new(pattern) {
                Ok(regex) => regex.is_match(&self.render(&when.value)),
                Err(error) => {
                    self.diverge(format!(
                        "when: {pattern:?} is not a regular expression: {error}"
                    ));
                    false
                }
            },
            (None, Some(expected), None) => self.render(&when.value) == *expected,
            (None, None, Some(name)) => self.variable(name).is_some(),
            (None, None, None)
            | (Some(_), Some(_), _)
            | (Some(_), None, Some(_))
            | (None, Some(_), Some(_)) => {
                self.diverge("when needs exactly one of matches, equals and bound");
                false
            }
        };
        self.run(if holds { &when.then } else { &when.otherwise })
            .await
    }

    pub(super) fn set(&self, set: &SetStep) -> Run {
        let value = Value::String(self.text(&set.value));
        self.live
            .record
            .state()
            .vars
            .insert(set.name.clone(), value);
        Ok(())
    }

    pub(super) fn commands(&self, step: &CommandsStep) -> Run {
        self.lane
            .sink
            .commands(available_commands(&step.commands))
            .map_err(|_| Break::Gone)
    }
}
