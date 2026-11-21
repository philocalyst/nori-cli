use std::collections::BTreeMap;

use pretty_assertions::assert_eq;
use serde_json::json;

use super::EvaluatorOptions;
use super::evaluate;
use super::evaluate_source_at_path;
use super::parse_document;

#[test]
fn evaluates_the_conversation_program_shapes() {
    let source = r#"
commands = new Listing {
  new Dynamic { op = "spawn"; actor = "reviewer"; agent = "claude" }
  new Dynamic { op = "send"; recipient = "reviewer"; message = "inspect" }
  new Dynamic { op = "tool-request"; id = "lookup"; tool = "git_status"; input = "{}" }
}
"#;
    let value: serde_json::Value = evaluate(source, &EvaluatorOptions::preconfigured()).unwrap();
    assert_eq!(
        value,
        json!({
            "commands": [
                {"op": "spawn", "actor": "reviewer", "agent": "claude"},
                {"op": "send", "recipient": "reviewer", "message": "inspect"},
                {"op": "tool-request", "id": "lookup", "tool": "git_status", "input": "{}"}
            ]
        })
    );
}

#[test]
fn evaluates_mappings_locals_and_machine_properties_without_runtime_processes() {
    let source = r#"
local available = read("prop:machine.cpu.available_parallelism")
name = "native"
fallback_ranked = available != "unsupported" && available.toInt() > 0
tools = new Dynamic {
  descriptions = new Mapping { ["get_goal"] = "Read the goal" }
}
"#;
    let options = EvaluatorOptions::preconfigured().properties(BTreeMap::from([(
        "machine.cpu.available_parallelism".into(),
        "8".into(),
    )]));
    let value: serde_json::Value = evaluate(source, &options).unwrap();
    assert_eq!(value["name"], "native");
    assert_eq!(value["fallback_ranked"], true);
    assert_eq!(value["tools"]["descriptions"]["get_goal"], "Read the goal");
}

#[test]
fn restrictive_capabilities_reject_environment_and_file_reads() {
    let options = EvaluatorOptions::new().allowed_modules(vec!["repl:".into()]);
    let environment = evaluate::<serde_json::Value>("value = read(\"env:HOME\")", &options);
    assert!(environment.is_err());
    let file = evaluate::<serde_json::Value>("value = read(\"file:///etc/passwd\")", &options);
    assert!(file.is_err());
}

#[test]
fn unsupported_constructs_report_an_error_instead_of_falling_back() {
    let result = evaluate::<serde_json::Value>(
        "value = read(\"unsupported:resource\")",
        &EvaluatorOptions::preconfigured(),
    );
    assert!(result.is_err());
    let message = result.unwrap_err();
    assert!(!message.is_empty());
}

#[test]
fn parser_retains_source_and_precise_member_spans_for_structural_editing() {
    let source = "// keep this\nname = \"before\"\n";
    let document = parse_document(source).unwrap();
    assert_eq!(document.source, source);
    let member = document
        .properties
        .iter()
        .find(|member| member.property.name.as_ref() == "name")
        .unwrap();
    assert_eq!(&source[member.value_span.clone().unwrap()], "\"before\"");
}

#[test]
fn evaluates_captured_source_with_relative_amendment() {
    let directory = tempfile::tempdir().unwrap();
    let base = directory.path().join("base.pkl");
    let child = directory.path().join("child.pkl");
    std::fs::write(&base, "result = \"from-base\"\nnumber = 2\n").unwrap();
    let source = "amends \"base.pkl\"\nresult = \"captured \" + number.toString()\n";
    std::fs::write(&child, "this source is intentionally replaced").unwrap();
    let value: serde_json::Value =
        evaluate_source_at_path(source, Some(&child), &EvaluatorOptions::preconfigured()).unwrap();
    assert_eq!(value["result"], "captured 2");
}

#[test]
fn evaluates_classes_lambdas_and_json_rendering() {
    let source = r#"
class Item { value: Int }
local doubled = List(1, 2, 3).map((item) -> item * 2)
values = doubled
item = new Item { value = 3 }
payload = new JsonRenderer { indent = "" }.renderValue(new Dynamic { op = "tool-request"; input = "{}" })
"#;
    let value: serde_json::Value = evaluate(source, &EvaluatorOptions::preconfigured()).unwrap();
    assert_eq!(value["values"], json!([2, 4, 6]));
    assert_eq!(value["item"], json!({"value":3}));
    assert_eq!(
        value["payload"],
        "{\"op\":\"tool-request\",\"input\":\"{}\"}"
    );
}

#[test]
fn evaluates_the_pkl_json_parser_module() {
    let source = r#"
import "pkl:json"
value = new json.Parser {}.parse("{\"answer\": 42}")
"#;
    let value: serde_json::Value = evaluate(source, &EvaluatorOptions::preconfigured()).unwrap();
    assert_eq!(value["value"]["answer"], 42);
}

#[test]
fn preserves_explicit_nulls_in_objects_and_lists() {
    let source = "value = null\nvalues = List(null, 1)\nobject = new Dynamic { missing = null }\n";
    let value: serde_json::Value = evaluate(source, &EvaluatorOptions::preconfigured()).unwrap();
    assert!(value["value"].is_null());
    assert_eq!(value["values"], json!([null, 1]));
    assert!(value["object"]["missing"].is_null());
}

#[test]
fn deferred_member_failure_is_reported_when_forced() {
    let source = "value: Listing<Int> = new Listing { \"not-an-int\" }\n";
    let error = evaluate::<serde_json::Value>(source, &EvaluatorOptions::preconfigured())
        .expect_err("forcing an invalid deferred conversion must fail");
    assert!(!error.is_empty());
}

#[test]
fn rejects_parentless_anonymous_new_while_keeping_typed_and_property_new() {
    for invalid in [
        "value = List(new { op = \"nop\" })\n",
        "function make() = new { op = \"nop\" }\nvalue = make()\n",
        "value: Any = new { op = \"nop\" }\n",
        "value = (if (true) new { op = \"nop\" } else new { op = \"nop\" })\n",
    ] {
        let err = evaluate::<serde_json::Value>(invalid, &EvaluatorOptions::preconfigured())
            .expect_err("parentless anonymous new must fail");
        assert!(
            err.contains("Cannot tell which parent to amend"),
            "{invalid}: {err}"
        );
    }

    let valid = r#"
class Step { kind: String = "literal"; text: String }
step: Step = new { text = "ok" }
untyped = new { ok = true }
"#;
    let value: serde_json::Value = evaluate(valid, &EvaluatorOptions::preconfigured()).unwrap();
    assert_eq!(value["step"], json!({"kind": "literal", "text": "ok"}));
    assert_eq!(value["untyped"], json!({"ok": true}));
}

#[test]
fn supports_nullable_typed_class_and_recursive_listing_defaults_plus_virtual_modules() {
    let base_schema = r#"
open module nori.Schema

class Node {
  name: String = "root"
  next: Node?
  children: Listing<Node>
}

node: Node?
items: Listing<Node>
"#;
    let child = r#"
amends "nori:Schema"

node = (super.node) {
  name = "head"
  next = (next) { name = "tail" }
}
items = (super.items) {
  new {
    name = "branch"
    children {
      new { name = "leaf" }
    }
  }
}
"#;
    let options =
        EvaluatorOptions::preconfigured().virtual_module("nori:Schema", base_schema.to_owned());
    let value: serde_json::Value = evaluate(child, &options).unwrap();
    assert_eq!(
        value,
        json!({
            "node": {
                "name": "head",
                "next": { "name": "tail", "next": null, "children": [] },
                "children": []
            },
            "items": [
                {
                    "name": "branch",
                    "next": null,
                    "children": [
                        { "name": "leaf", "next": null, "children": [] }
                    ]
                }
            ]
        })
    );
}

#[test]
fn an_error_inside_a_local_is_reported_at_the_local() {
    let base = "module Base\nname: String\nactors: Mapping<String, Dynamic> = new Mapping {}\n";
    let cases = [
        (
            "local x = throw(\"boom\")\nname = x\n",
            "boom\n  --> repl:text:1:11\n   | local x = throw(\"boom\")\n   |           ^^^^^^^^^^^^^\n  in local `x`",
        ),
        (
            "o {\n  local x: Int(this > 5) = 1\n  name = x\n}\n",
            "Type constraint `this > 5` violated.\nValue: 1\n  --> repl:text:2:28\n   |   local x: Int(this > 5) = 1\n   |                            ^\n  in local `x`",
        ),
        (
            "local x = nothere + 1\nname = x\n",
            "Cannot find property `nothere`.\n  --> repl:text:1:11\n   | local x = nothere + 1\n   |           ^^^^^^^\n  in local `x`",
        ),
        (
            "amends \"nori:Base\"\nlocal members = throw(\"boom\")\nname = \"n\"\nactors { for (i, m in members) { [\"m\\(i)\"] { agent = m } } }\n",
            "boom\n  --> repl:text:2:17\n   | local members = throw(\"boom\")\n   |                 ^^^^^^^^^^^^^\n  in local `members`",
        ),
    ];
    for (source, expected) in cases {
        let options = EvaluatorOptions::preconfigured().virtual_module("nori:Base", base);
        let error = evaluate::<serde_json::Value>(source, &options).unwrap_err();
        assert_eq!(error, expected);
    }
}

#[test]
fn a_parameter_default_in_a_mapping_entry_body_is_kept() {
    let plugin = "open module nori.Plugin\n\
        class Parameter {\n  description: String = \"\"\n  default: String?\n}\n\
        class Command {\n  parameters: Mapping<String, Parameter>\n}\n\
        commands: Mapping<String, Command>\n";
    let source = "amends \"nori:Plugin\"\ncommands { [\"to\"] { parameters { [\"machine\"] { description = \"d\"; default = \"h1\" } } } }\n";
    let options = EvaluatorOptions::preconfigured().virtual_module("nori:Plugin", plugin);
    let value: serde_json::Value = evaluate(source, &options).unwrap();
    assert_eq!(
        value["commands"]["to"]["parameters"]["machine"],
        json!({"description": "d", "default": "h1"})
    );
}
