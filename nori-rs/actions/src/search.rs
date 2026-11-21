//! Finding actions by what the user types in the palette.

use nucleo_matcher::Config;
use nucleo_matcher::Matcher;
use nucleo_matcher::Utf32Str;
use nucleo_matcher::pattern::CaseMatching;
use nucleo_matcher::pattern::Normalization;
use nucleo_matcher::pattern::Pattern;

use crate::Action;
use crate::Registry;
use crate::Situation;

/// An action the query found.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Hit<'a> {
    pub action: &'a Action,
    /// Why it cannot run now, if it cannot.
    pub unavailable: Option<&'static str>,
}

impl Registry {
    /// The actions matching `query`, best first: an exact name, then names starting with the
    /// query (in registration order, which puts the common ones first), then fuzzy matches on
    /// the title, names and keywords. An empty query lists every
    /// action in registration order. Unavailable actions come after available ones.
    pub fn search(&self, query: &str, situation: &Situation) -> Vec<Hit<'_>> {
        let query = query.trim();
        let mut scored: Vec<(i64, usize, &Action)> = Vec::new();
        if query.is_empty() {
            scored.extend(
                self.actions()
                    .iter()
                    .enumerate()
                    .map(|(order, action)| (0, order, action)),
            );
        } else {
            let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
            let mut matcher = Matcher::new(Config::DEFAULT);
            let mut buffer = Vec::new();
            let lower = query.to_lowercase();
            for (order, action) in self.actions().iter().enumerate() {
                let name_rank = action
                    .names
                    .iter()
                    .map(|name| {
                        let name = name.to_lowercase();
                        if name == lower {
                            3_000_000
                        } else if name.starts_with(&lower) {
                            2_000_000
                        } else {
                            0
                        }
                    })
                    .max()
                    .unwrap_or(0);
                let haystack = format!(
                    "{} {} {} {}",
                    action.names.join(" "),
                    action.title,
                    action.keywords.join(" "),
                    action.category.label()
                );
                let fuzzy = pattern
                    .score(Utf32Str::new(&haystack, &mut buffer), &mut matcher)
                    .map(i64::from);
                let score = match (name_rank, fuzzy) {
                    (0, None) => continue,
                    (0, Some(fuzzy)) => fuzzy,
                    (rank, _) => rank,
                };
                scored.push((score, order, action));
            }
        }
        let mut hits: Vec<(bool, i64, usize, Hit<'_>)> = scored
            .into_iter()
            .map(|(score, order, action)| {
                let unavailable = action.requires.check(situation).err();
                (
                    unavailable.is_some(),
                    -score,
                    order,
                    Hit {
                        action,
                        unavailable,
                    },
                )
            })
            .collect();
        hits.sort_by_key(|(unavailable, score, order, _)| (*unavailable, *score, *order));
        hits.into_iter().map(|(.., hit)| hit).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Category;
    use crate::Requires;
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
            .named(&["model"]),
        );
        registry.register(
            Action::new(
                "session.move",
                "Move session",
                "to another machine",
                Category::Machine,
            )
            .named(&["move"])
            .keywords(&["remote", "ssh"])
            .requires(Requires {
                idle: true,
                ..Requires::default()
            }),
        );
        registry.register(
            Action::new(
                "conversation.compact",
                "Compact",
                "summarize",
                Category::Context,
            )
            .named(&["compact"]),
        );
        registry
    }

    fn ids(hits: &[Hit<'_>]) -> Vec<String> {
        hits.iter().map(|hit| hit.action.id.to_string()).collect()
    }

    #[test]
    fn names_first_then_fuzzy_then_keywords() {
        let registry = registry();
        let idle = Situation::default();
        // Names starting with the query first; fuzzy matches (`compact … context`) after.
        assert_eq!(
            ids(&registry.search("mo", &idle)),
            vec!["model.set", "session.move", "conversation.compact"]
        );
        assert_eq!(ids(&registry.search("ssh", &idle)), vec!["session.move"]);
        assert_eq!(ids(&registry.search("", &idle)).len(), 3);
    }

    #[test]
    fn unavailable_actions_sink_and_say_why() {
        let registry = registry();
        let busy = Situation {
            turn_running: true,
            ..Situation::default()
        };
        let hits = registry.search("m", &busy);
        let last = hits.last().expect("hits");
        assert_eq!(last.action.id.as_str(), "session.move");
        assert_eq!(
            last.unavailable,
            Some("waits for the running turn to finish")
        );
    }
}
