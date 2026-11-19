//! Values retain Pkl's property/entry and definition-site class distinctions.
//! Expected text is checked against the pinned official Pkl 0.33 evaluator.

use super::*;

#[test]
fn parsed_json_dynamic_members_are_properties() {
    let actual = eval(
        r#"
import "pkl:json"
local parsed = new json.Parser {}.parse("{\"plain\":1,\"entry key\":2}")
result = new PcfRenderer {}.renderValue(parsed)
"#,
    );
    assert_eq!(
        actual,
        serde_json::json!({
            "result": "{\n  plain = 1\n  `entry key` = 2\n}"
        })
    );
}

#[test]
fn parsed_dynamic_amendments_preserve_properties_and_explicit_entries() {
    let actual = eval(
        r#"
import "pkl:json"
local parsed = new json.Parser {}.parse("{\"original\":1}")
local amended = (parsed) { added = 2; ["entry"] = 3 }
result = new PcfRenderer {}.renderValue(amended)
"#,
    );
    assert_eq!(
        actual,
        serde_json::json!({
            "result": "{\n  original = 1\n  added = 2\n  [\"entry\"] = 3\n}"
        })
    );
}

#[test]
fn parsed_dynamic_keys_are_not_converted_as_mapping_entry_keys() {
    let actual = eval(
        r#"
import "pkl:json"
local parsed = new json.Parser {}.parse("{\"name\":\"pigeon\",\"nested\":{\"name\":\"owl\"}}")
local amended = (parsed) { ["entry"] = "robin" }
result = new JsonRenderer {
  indent = ""
  converters { [String] = (it) -> it.toUpperCase() }
}.renderValue(amended)
"#,
    );
    assert_eq!(
        actual,
        serde_json::json!({
            "result": "{\"name\":\"PIGEON\",\"nested\":{\"name\":\"OWL\"},\"ENTRY\":\"ROBIN\"}"
        })
    );
}

#[test]
fn parsed_mapping_keys_remain_entries() {
    let actual = eval(
        r#"
import "pkl:json"
local parsed = new json.Parser { useMapping = true }.parse("{\"name\":\"pigeon\"}")
result = new PcfRenderer {}.renderValue(parsed)
converted = new JsonRenderer {
  indent = ""
  converters { [String] = (it) -> it.toUpperCase() }
}.renderValue(parsed)
"#,
    );
    assert_eq!(
        actual,
        serde_json::json!({
            "result": "{\n  [\"name\"] = \"pigeon\"\n}",
            "converted": "{\"NAME\":\"PIGEON\"}"
        })
    );
}

#[test]
fn yaml_document_streams_are_lists_while_document_sequences_are_listings() {
    let actual = eval(
        r#"
import "pkl:yaml"
local parser = new yaml.Parser {}
empty = new PcfRenderer {}.renderValue(parser.parseAll(""))
documents = new PcfRenderer {}.renderValue(parser.parseAll("---\n1\n---\n2\n"))
streamClass = parser.parseAll("---\n1\n").getClass().simpleName
sequenceClass = parser.parse("[1, 2]").getClass().simpleName
"#,
    );
    assert_eq!(
        actual,
        serde_json::json!({
            "empty": "List()", "documents": "List(1, 2)",
            "streamClass": "List", "sequenceClass": "Listing"
        })
    );
}

#[test]
fn typed_object_text_uses_its_definition_site_class_name() {
    let actual = eval(
        r#"
import "pkl:test"
class Bird {}
class Age { age: Int = 2 }
text = new Age {}.toString()
pipeError = test.catch(() -> 1 |> new Bird {})
renderError = test.catch(() -> new PropertiesRenderer {}.renderValue(new Bird {}))
"#,
    );
    assert_eq!(
        actual,
        serde_json::json!({
            "text": "new test#Age { age = 2 }",
            "pipeError": "Operator `|>` is not defined for operand types `Int` and `test#Bird`. Left operand : 1 Right operand: new test#Bird {}",
            "renderError": "Cannot render value of type `test#Bird` as Properties. Value: new test#Bird {}"
        })
    );
}

#[test]
fn declared_module_names_qualify_typed_object_text() {
    let actual = eval(
        r#"
module birds.caged
class Bird { age = 1 }
local original = new Bird {}
text = original.toString()
amended = (original) { age = 2 }.toString()
amendedClass = (original) { age = 3 }.getClass().toString()
sameClass = (original) { age = 4 }.getClass() == Bird
"#,
    );
    assert_eq!(
        actual,
        serde_json::json!({
            "text": "new birds.caged#Bird { age = 1 }",
            "amended": "new birds.caged#Bird { age = 2 }",
            "amendedClass": "birds.caged#Bird",
            "sameClass": true
        })
    );
}

#[test]
fn standard_module_class_names_use_the_language_module_name() {
    let actual = eval(
        r#"
import "pkl:json"
text = new json.Parser {}.toString()
classText = new json.Parser {}.getClass().toString()
"#,
    );
    assert_eq!(
        actual,
        serde_json::json!({
            "text": "new pkl.json#Parser { useMapping = false; converters {} }",
            "classText": "pkl.json#Parser"
        })
    );
}

#[test]
fn pair_text_uses_the_constructor_form_and_qualifies_nested_objects() {
    let actual = eval(
        r#"
class Bird {}
simple = Pair(1, 2).toString()
nested = Pair("first", Pair(new Bird {}, 3)).toString()
"#,
    );
    assert_eq!(
        actual,
        serde_json::json!({
            "simple": "Pair(1, 2)",
            "nested": "Pair(\"first\", Pair(new test#Bird {}, 3))"
        })
    );
}

#[test]
fn identical_display_names_do_not_merge_distinct_imported_class_identities() {
    let directory = TestTempDir::new("pklr-class-display");
    for name in ["first", "second"] {
        std::fs::write(
            directory.path().join(format!("{name}.pkl")),
            "module birds.caged\nclass Bird {}\n",
        )
        .unwrap();
    }
    let path = directory.path().join("test.pkl");
    std::fs::write(
        &path,
        r#"
import "first.pkl"
import "second.pkl"
left = new first.Bird {}.toString()
right = new second.Bird {}.toString()
sameClass = first.Bird == second.Bird
sameInstanceType = new first.Bird {} is second.Bird
"#,
    )
    .unwrap();
    let actual = Evaluator::new().eval_file(&path).unwrap().to_json();
    assert_eq!(
        actual,
        serde_json::json!({
            "left": "new birds.caged#Bird {}", "right": "new birds.caged#Bird {}",
            "sameClass": false, "sameInstanceType": false
        })
    );
}
