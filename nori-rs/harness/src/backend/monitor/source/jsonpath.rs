//! The part of JSONPath a status endpoint needs: `$`, `.key`, `['key']`, `[n]` (negative counts
//! from the end), `[*]`, `.*` and `..key`.

use serde_json::Value;

#[derive(Debug, PartialEq, Eq)]
enum Step {
    Key(String),
    Index(i64),
    Every,
    Descend(String),
}

/// Every value `expression` selects in `document`.
pub(super) fn select<'a>(document: &'a Value, expression: &str) -> Result<Vec<&'a Value>, String> {
    let mut current = vec![document];
    for step in parse(expression)? {
        let mut next = Vec::new();
        for value in current {
            match &step {
                Step::Key(key) => next.extend(value.get(key)),
                Step::Index(index) => {
                    if let Value::Array(items) = value {
                        let at = if *index < 0 {
                            i64::try_from(items.len()).unwrap_or(0) + index
                        } else {
                            *index
                        };
                        next.extend(usize::try_from(at).ok().and_then(|at| items.get(at)));
                    }
                }
                Step::Every => match value {
                    Value::Array(items) => next.extend(items),
                    Value::Object(map) => next.extend(map.values()),
                    Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
                },
                Step::Descend(key) => descend(value, key, &mut next),
            }
        }
        current = next;
    }
    Ok(current)
}

fn descend<'a>(value: &'a Value, key: &str, out: &mut Vec<&'a Value>) {
    match value {
        Value::Object(map) => {
            out.extend(map.get(key));
            for child in map.values() {
                descend(child, key, out);
            }
        }
        Value::Array(items) => {
            for child in items {
                descend(child, key, out);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn parse(expression: &str) -> Result<Vec<Step>, String> {
    let bad = |why: &str| format!("`json_path` {expression:?} is not understood: {why}");
    let text = expression.trim();
    let mut rest = text
        .strip_prefix('$')
        .ok_or_else(|| bad("it must start with `$`"))?;
    let mut steps = Vec::new();
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("..") {
            let end = after.find(['.', '[']).unwrap_or(after.len());
            if end == 0 {
                return Err(bad("`..` needs a name"));
            }
            steps.push(Step::Descend(after[..end].to_owned()));
            rest = &after[end..];
        } else if let Some(after) = rest.strip_prefix('.') {
            let end = after.find(['.', '[']).unwrap_or(after.len());
            let name = &after[..end];
            if name.is_empty() {
                return Err(bad("`.` needs a name"));
            }
            steps.push(if name == "*" {
                Step::Every
            } else {
                Step::Key(name.to_owned())
            });
            rest = &after[end..];
        } else if let Some(after) = rest.strip_prefix('[') {
            let end = after.find(']').ok_or_else(|| bad("a `[` is not closed"))?;
            let inner = after[..end].trim();
            steps.push(if inner == "*" {
                Step::Every
            } else if let Some(quoted) = inner
                .strip_prefix('\'')
                .and_then(|inner| inner.strip_suffix('\''))
                .or_else(|| {
                    inner
                        .strip_prefix('"')
                        .and_then(|inner| inner.strip_suffix('"'))
                })
            {
                Step::Key(quoted.to_owned())
            } else {
                Step::Index(
                    inner
                        .parse()
                        .map_err(|_| bad("brackets hold a number, `*` or a quoted name"))?,
                )
            });
            rest = &after[end + 1..];
        } else {
            return Err(bad("expected `.name`, `[n]`, `[*]` or `..name`"));
        }
    }
    Ok(steps)
}

/// A selected value as one line of text.
pub(super) fn render(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}
