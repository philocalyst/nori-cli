//! Re-indenting replacement text to the file's indentation.
//!
//! When an edit only matched after ignoring indentation, the model's `new_string` carries the
//! wrong indentation too. The plan computed here maps the model's indentation onto the file's, so
//! the replacement lands at the right depth, in the file's own tabs or spaces.

/// Leading whitespace of a line.
pub fn leading_ws(line: &str) -> &str {
    let end = line
        .find(|c: char| c != ' ' && c != '\t')
        .unwrap_or(line.len());
    &line[..end]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Convert {
    SpacesToTabs { unit: usize },
    TabsToSpaces { unit: usize },
}

/// How to turn the model's indentation into the file's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndentPlan {
    convert: Option<Convert>,
    strip: String,
    add: String,
    reconciled: bool,
}

impl Convert {
    fn apply(self, ws: &str) -> String {
        match self {
            Self::SpacesToTabs { unit } => {
                if ws.chars().all(|c| c == ' ') {
                    let levels = ws.len() / unit;
                    let rest = ws.len() % unit;
                    format!("{}{}", "\t".repeat(levels), " ".repeat(rest))
                } else {
                    ws.to_owned()
                }
            }
            Self::TabsToSpaces { unit } => {
                let tabs = ws.chars().take_while(|c| *c == '\t').count();
                format!("{}{}", " ".repeat(tabs * unit), &ws[tabs..])
            }
        }
    }
}

fn direct(needle_ws: &[String], file_ws: &[&str]) -> Option<(String, String)> {
    let (needle0, file0) = (needle_ws.first()?, file_ws.first()?);
    if let Some(prefix) = file0.strip_suffix(needle0.as_str()) {
        let consistent = needle_ws
            .iter()
            .zip(file_ws)
            .all(|(needle, file)| file.strip_prefix(prefix) == Some(needle.as_str()));
        return consistent.then(|| (String::new(), prefix.to_owned()));
    }
    if let Some(prefix) = needle0.strip_suffix(file0) {
        let consistent = needle_ws
            .iter()
            .zip(file_ws)
            .all(|(needle, file)| needle.strip_prefix(prefix) == Some(*file));
        return consistent.then(|| (prefix.to_owned(), String::new()));
    }
    None
}

fn smallest_space_indent<'a>(lines: impl Iterator<Item = &'a str>) -> Option<usize> {
    lines
        .filter(|line| !line.trim().is_empty())
        .map(|line| leading_ws(line).len())
        .filter(|width| *width > 0)
        .min()
}

/// Plan how to re-indent text written against `needle` lines so it fits the matched `file` lines
/// (parallel slices; blank pairs are ignored). `new_text` is consulted to guess the indent unit.
pub fn plan(needle: &[&str], file: &[&str], new_text: &str) -> IndentPlan {
    let pairs: Vec<(&str, &str)> = needle
        .iter()
        .zip(file)
        .filter(|(n, f)| !n.trim().is_empty() && !f.trim().is_empty())
        .map(|(n, f)| (leading_ws(n), leading_ws(f)))
        .collect();
    let needle_ws: Vec<String> = pairs.iter().map(|(n, _)| (*n).to_owned()).collect();
    let file_ws: Vec<&str> = pairs.iter().map(|(_, f)| *f).collect();

    if let Some((strip, add)) = direct(&needle_ws, &file_ws) {
        return IndentPlan {
            convert: None,
            strip,
            add,
            reconciled: true,
        };
    }

    let needle_has_tab = needle_ws.iter().any(|ws| ws.contains('\t'));
    let file_has_tab = file_ws.iter().any(|ws| ws.contains('\t'));
    let convert = if !needle_has_tab && file_has_tab {
        let unit = smallest_space_indent(needle.iter().copied().chain(new_text.split('\n')));
        Some(Convert::SpacesToTabs {
            unit: unit.unwrap_or(4),
        })
    } else if needle_has_tab && !file_has_tab {
        Some(Convert::TabsToSpaces {
            unit: smallest_space_indent(file.iter().copied()).unwrap_or(4),
        })
    } else {
        None
    };
    if let Some(convert) = convert {
        let converted: Vec<String> = needle_ws.iter().map(|ws| convert.apply(ws)).collect();
        if let Some((strip, add)) = direct(&converted, &file_ws) {
            return IndentPlan {
                convert: Some(convert),
                strip,
                add,
                reconciled: true,
            };
        }
    }
    IndentPlan {
        convert: None,
        strip: String::new(),
        add: String::new(),
        reconciled: false,
    }
}

impl IndentPlan {
    /// Re-indent every non-blank line of `text`.
    pub fn apply(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len() + 16);
        for (index, line) in text.split('\n').enumerate() {
            if index > 0 {
                out.push('\n');
            }
            if line.trim().is_empty() {
                out.push_str(line);
                continue;
            }
            let ws = leading_ws(line);
            let body = &line[ws.len()..];
            let converted = match self.convert {
                Some(convert) => convert.apply(ws),
                None => ws.to_owned(),
            };
            let stripped = converted
                .strip_prefix(self.strip.as_str())
                .unwrap_or(&converted);
            out.push_str(&self.add);
            out.push_str(stripped);
            out.push_str(body);
        }
        out
    }

    /// A short description, or `None` when the text is inserted unchanged.
    pub fn describe(&self) -> Option<String> {
        if !self.reconciled {
            return Some(
                "the indentation could not be reconciled, so the new text was inserted as \
                 written; check it"
                    .to_owned(),
            );
        }
        let mut parts = Vec::new();
        match self.convert {
            Some(Convert::SpacesToTabs { .. }) => {
                parts.push("converted spaces to tabs".to_owned());
            }
            Some(Convert::TabsToSpaces { .. }) => {
                parts.push("converted tabs to spaces".to_owned());
            }
            None => {}
        }
        if !self.add.is_empty() {
            parts.push(format!("+{}", describe_ws(&self.add)));
        }
        if !self.strip.is_empty() {
            parts.push(format!("-{}", describe_ws(&self.strip)));
        }
        if parts.is_empty() {
            None
        } else {
            Some(format!("re-indented ({})", parts.join(", ")))
        }
    }
}

pub(crate) fn describe_ws(ws: &str) -> String {
    let tabs = ws.chars().filter(|c| *c == '\t').count();
    let spaces = ws.chars().filter(|c| *c == ' ').count();
    match (tabs, spaces) {
        (0, 1) => "1 space".to_owned(),
        (0, n) => format!("{n} spaces"),
        (1, 0) => "1 tab".to_owned(),
        (n, 0) => format!("{n} tabs"),
        (t, s) => format!("{t} tabs and {s} spaces"),
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn adds_a_common_prefix() {
        let plan = plan(
            &["fn a() {", "    x"],
            &["    fn a() {", "        x"],
            "fn b() {\n    y",
        );
        assert_eq!(plan.apply("fn b() {\n    y"), "    fn b() {\n        y");
        assert_eq!(plan.describe().as_deref(), Some("re-indented (+4 spaces)"));
    }

    #[test]
    fn strips_a_common_prefix() {
        let plan = plan(&["    a", "      b"], &["a", "  b"], "    c");
        assert_eq!(plan.apply("    c\n\n      d"), "c\n\n  d");
    }

    #[test]
    fn converts_spaces_to_tabs() {
        let plan = plan(
            &["if x {", "    y"],
            &["if x {", "\ty"],
            "if z {\n    w\n        v",
        );
        assert_eq!(plan.apply("if z {\n    w\n        v"), "if z {\n\tw\n\t\tv");
        assert_eq!(
            plan.describe().as_deref(),
            Some("re-indented (converted spaces to tabs)")
        );
    }

    #[test]
    fn converts_tabs_to_spaces() {
        let plan = plan(&["a", "\tb"], &["a", "  b"], "a\n\tc");
        assert_eq!(plan.apply("a\n\tc"), "a\n  c");
    }

    #[test]
    fn inconsistent_indentation_is_reported_not_guessed() {
        let plan = plan(&["  a", "  b"], &["    a", "  b"], "  c");
        assert_eq!(plan.apply("  c"), "  c");
        assert!(
            plan.describe()
                .is_some_and(|d| d.contains("could not be reconciled"))
        );
    }
}
