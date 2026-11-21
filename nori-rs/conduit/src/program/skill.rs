//! Skills (`nori:Skill`): programs with parameters. Running one fixes its arguments first, so the
//! program that is checked and run has them in place of `{{parameter}}`, as constants.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::error::ConduitError;

/// Put `arguments` (and the defaults of parameters not given) in place of `{{parameter}}` in every
/// string of the evaluated program `value`. A program with no `parameters` is left alone, and
/// then any argument is an error.
pub(super) fn apply_arguments(
    value: &mut Value,
    arguments: &BTreeMap<String, String>,
    file: &str,
) -> Result<(), ConduitError> {
    let declared: BTreeMap<String, Value> = value
        .get("parameters")
        .and_then(Value::as_object)
        .map(|parameters| {
            parameters
                .iter()
                .map(|(name, parameter)| (name.clone(), parameter.clone()))
                .collect()
        })
        .unwrap_or_default();
    let unknown: Vec<&str> = arguments
        .keys()
        .filter(|name| !declared.contains_key(*name))
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        let known = declared.keys().cloned().collect::<Vec<_>>().join(", ");
        return Err(ConduitError::Compilation {
            message: format!(
                "{file} takes no argument {}; its parameters are: {}",
                unknown.join(", "),
                if known.is_empty() { "none" } else { &known }
            ),
        });
    }
    let mut values = BTreeMap::new();
    let mut missing = Vec::new();
    for (name, parameter) in &declared {
        let default = parameter.get("default").and_then(Value::as_str);
        let required = parameter
            .get("required")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        match (arguments.get(name), default) {
            (Some(argument), _) => {
                values.insert(name.clone(), argument.clone());
            }
            (None, Some(default)) => {
                values.insert(name.clone(), default.to_owned());
            }
            (None, None) if required => missing.push(name.as_str()),
            // An optional parameter with no value is empty text.
            (None, None) => {
                values.insert(name.clone(), String::new());
            }
        }
    }
    if !missing.is_empty() {
        return Err(ConduitError::Compilation {
            message: format!("{file} needs the argument(s) {}", missing.join(", ")),
        });
    }
    if values.is_empty() {
        return Ok(());
    }
    if let Value::Object(fields) = value {
        for (key, field) in fields.iter_mut() {
            if key != "parameters" {
                substitute(field, &values);
            }
        }
    }
    Ok(())
}

fn substitute(value: &mut Value, values: &BTreeMap<String, String>) {
    match value {
        Value::String(text) => {
            for (name, argument) in values {
                let reference = format!("{{{{{name}}}}}");
                if text.contains(&reference) {
                    // An argument that itself says `{{` is literal text, not a reference.
                    *text = text.replace(&reference, &argument.replace("{{", "\\{{"));
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                substitute(item, values);
            }
        }
        Value::Object(fields) => {
            for field in fields.values_mut() {
                substitute(field, values);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn arguments_and_defaults_take_the_place_of_their_references() {
        let mut program = json!({
            "name": "council",
            "parameters": {
                "question": {"description": "", "required": true},
                "rounds": {"description": "", "default": "2", "required": true}
            },
            "steps": [{"kind": "send", "to": "a", "message": "{{question}} in {{rounds}} rounds; {{answer}}"}]
        });
        let arguments = BTreeMap::from([("question".to_owned(), "Tabs or {{spaces}}?".to_owned())]);
        apply_arguments(&mut program, &arguments, "council.pkl").expect("applies");
        assert_eq!(
            program["steps"][0]["message"],
            json!("Tabs or \\{{spaces}}? in 2 rounds; {{answer}}")
        );
    }

    #[test]
    fn unknown_and_missing_arguments_are_named() {
        let mut program = json!({"parameters": {"question": {"required": true}}});
        let unknown = BTreeMap::from([("topic".to_owned(), "x".to_owned())]);
        let error = apply_arguments(&mut program.clone(), &unknown, "c.pkl")
            .expect_err("unknown")
            .to_string();
        assert!(
            error.contains("no argument topic") && error.contains("question"),
            "{error}"
        );
        let error = apply_arguments(&mut program, &BTreeMap::new(), "c.pkl")
            .expect_err("missing")
            .to_string();
        assert!(error.contains("needs the argument(s) question"), "{error}");
    }
}
