//! An error inside a `local` reaches the person as that error, never as a missing name, and a
//! property named `default` in a Mapping entry's body is a property of the entry.

use std::path::Path;

use pretty_assertions::assert_eq;

const BASE: &str = "module Base\n\
name: String\n\
actors: Mapping<String, Dynamic> = new Mapping {}\n\
list: Listing<String> = new Listing {}\n";

fn eval(source: &str) -> Result<serde_json::Value, String> {
    let mut evaluator = pklr::EvaluatorBuilder::new()
        .allowed_modules(["repl:".to_string(), "nori:".to_string(), "pkl:".to_string()])
        .virtual_modules([("nori:Base".to_string(), BASE.to_string())])
        .build();
    evaluator
        .eval_source_data(source, Path::new("repl:text"))
        .map_err(|error| error.to_string())
}

fn message(source: &str) -> String {
    eval(source).expect_err("the module must fail")
}

fn local_error(local: &str, use_site: &str) -> String {
    message(&format!(
        "amends \"nori:Base\"\n{local}\nname = \"n\"\n{use_site}\n"
    ))
}

#[test]
fn a_failed_local_read_by_a_for_generator_of_an_amended_mapping_keeps_its_error() {
    let for_loop = "actors { for (x in m) { [x] { a = 1 } } }";
    assert_eq!(
        local_error("local m = throw(\"boom\")", for_loop),
        "Eval error: boom"
    );
    assert_eq!(
        local_error("local m = nothere", for_loop),
        "Eval error: Cannot find property `nothere`."
    );
    assert_eq!(
        local_error("local m = read(\"file:///nonexistent/zzz\")", for_loop),
        "Eval error: Cannot find resource `file:///nonexistent/zzz`."
    );
}

#[test]
fn a_failed_local_read_in_a_mapping_entry_keeps_its_error() {
    let entry = "actors { [\"k\"] { a = m } }";
    assert_eq!(
        local_error("local m = throw(\"boom\")", entry),
        "Eval error: boom"
    );
    assert_eq!(
        local_error("local m: Int(this > 5) = 1", entry),
        "Eval error: Type constraint `this > 5` violated.\nValue: 1"
    );
}

#[test]
fn a_missing_name_is_still_a_missing_name() {
    assert_eq!(
        local_error("", "actors { for (x in nothere) { [x] { a = 1 } } }"),
        "Eval error: Cannot find property `nothere`."
    );
}

const PLUGIN: &str = "open module nori.Plugin\n\
class Parameter {\n  description: String = \"\"\n  default: String?\n  required: Boolean = true\n}\n\
class Command {\n  parameters: Mapping<String, Parameter>\n}\n\
name: String\n\
commands: Mapping<String, Command>\n";

#[test]
fn a_property_named_default_in_a_typed_mapping_entry_body_is_a_property() {
    let mut evaluator = pklr::EvaluatorBuilder::new()
        .allowed_modules(["repl:".to_string(), "nori:".to_string()])
        .virtual_modules([("nori:Plugin".to_string(), PLUGIN.to_string())])
        .build();
    let source = "amends \"nori:Plugin\"\nname = \"p\"\ncommands { [\"to\"] { parameters { [\"machine\"] { description = \"d\"; default = \"h1\" } } } }\n";
    let value = evaluator
        .eval_source_data(source, Path::new("repl:text"))
        .unwrap();
    assert_eq!(
        value["commands"]["to"]["parameters"]["machine"],
        serde_json::json!({"description": "d", "default": "h1", "required": true})
    );
}
