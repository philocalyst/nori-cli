//! Abstract-method obligations that can be resolved within a single module.
//! Imported parent declarations are checked by the evaluator when resolved.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use super::{Declaration, Modifier};

pub(super) fn violation(declarations: &[Declaration], module_name: &str) -> Option<String> {
    let classes = declarations
        .iter()
        .filter(|declaration| declaration.is_class && !declaration.nested)
        .map(|declaration| (declaration.name.as_ref(), declaration))
        .collect::<BTreeMap<_, _>>();
    for declaration in declarations
        .iter()
        .filter(|declaration| declaration.is_class)
    {
        if declaration.modifiers.contains(&Modifier::Abstract) {
            continue;
        }
        if declaration
            .bodyless_methods
            .iter()
            .any(|method| method.modifiers.contains(&Modifier::Abstract))
        {
            let container = if declaration.name.as_ref() == "module" {
                "module"
            } else {
                "class"
            };
            return Some(format!(
                "Cannot define an abstract method in a non-abstract {container}."
            ));
        }
        // Nested classes may reuse a name in another lexical class. Their
        // inherited declarations need lexical resolution in the evaluator.
        if declaration.nested {
            continue;
        }
        let missing = obligations(declaration, &classes, &mut BTreeSet::new());
        if missing.is_empty() {
            continue;
        }
        let identity = if declaration.name.as_ref() == "module" {
            module_name.to_string()
        } else {
            format!("{module_name}#{}", declaration.name)
        };
        let methods = missing
            .iter()
            .map(|name| format!("`{name}()`"))
            .collect::<Vec<_>>();
        let message = if let [method] = methods.as_slice() {
            format!(
                "Class `{identity}` should either be declared `abstract`, or should implement method {method}."
            )
        } else {
            format!(
                "Class `{identity}` should either be declared `abstract`, or should implement the following methods:\n{}",
                methods.join("\n")
            )
        };
        return Some(message);
    }
    None
}

fn obligations(
    declaration: &Declaration,
    classes: &BTreeMap<&str, &Declaration>,
    visiting: &mut BTreeSet<Arc<str>>,
) -> BTreeSet<Arc<str>> {
    if !visiting.insert(Arc::clone(&declaration.name)) {
        // Inheritance cycles have their own diagnostic.
        return BTreeSet::new();
    }
    let mut missing = declaration
        .parent
        .as_deref()
        .and_then(|parent| classes.get(parent))
        .map_or_else(BTreeSet::new, |parent| {
            obligations(parent, classes, visiting)
        });
    for signature in &declaration.signatures {
        missing.remove(&signature.name);
    }
    for method in &declaration.bodyless_methods {
        if method.modifiers.contains(&Modifier::Abstract) {
            missing.insert(Arc::clone(&method.name));
        }
    }
    visiting.remove(&declaration.name);
    missing
}

#[cfg(test)]
mod tests {
    use crate::{lexer, parser};

    #[test]
    fn non_abstract_classes_must_implement_inherited_methods() {
        let source = "abstract class A { abstract function f(): Int }\nclass B extends A {}\nx=1";
        let error = parser::parse_named(&lexer::lex(source).unwrap(), source, "test.pkl")
            .unwrap_err()
            .to_string();
        assert!(error.contains("should implement method `f()`"), "{error}");
    }

    #[test]
    fn intermediate_abstract_classes_retain_obligations() {
        let source = "abstract class A { abstract function f(): Int }\nabstract class B extends A {}\nclass C extends B {}\nx=1";
        assert!(parser::parse_named(&lexer::lex(source).unwrap(), source, "test.pkl").is_err());
    }

    #[test]
    fn concrete_implementations_satisfy_obligations() {
        let source = "abstract class A { abstract function f(): Int }\nopen class B extends A { function f(): Int = 42 }\nclass C extends B {}\nx=1";
        assert!(parser::parse_named(&lexer::lex(source).unwrap(), source, "test.pkl").is_ok());
    }

    #[test]
    fn non_abstract_module_cannot_declare_abstract_methods() {
        let source = "abstract function f(): Int";
        let error = parser::parse_named(&lexer::lex(source).unwrap(), source, "test.pkl")
            .unwrap_err()
            .to_string();
        assert!(error.contains("non-abstract module"), "{error}");
    }
}
