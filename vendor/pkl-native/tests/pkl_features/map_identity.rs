//! Map keys and object kinds remain language values throughout built-in methods.
//! Expected outputs are differential probes against official Pkl 0.33.0.

use super::*;
use pretty_assertions::assert_eq;

#[test]
fn map_text_uses_constructor_syntax_and_original_key_values() {
    assert_eq!(
        eval(
            r#"
a = Map(1, 2, "one", List(1), null, 3).toString()
b = List(Map(true, 3)).toString()
c = "\(Map(1.s, 2))"
d = Map().toString()
"#
        ),
        serde_json::json!({
            "a": "Map(1, 2, \"one\", List(1), null, 3)",
            "b": "List(Map(true, 3))", "c": "Map(1.s, 2)", "d": "Map()"
        })
    );
}

#[test]
fn mapping_and_dynamic_text_preserve_entry_and_property_kinds() {
    assert_eq!(
        eval(
            r#"
a = new Mapping { [1] = 2; ["x"] = new Dynamic { name = "owl" } }.toString()
b = new Dynamic { `a b` = 1; ["entry"] = List(2); [false] = Map(1, 2) }.toString()
"#
        ),
        serde_json::json!({
            "a": "new Mapping { [1] = 2; [\"x\"] { name = \"owl\" } }",
            "b": "new Dynamic { `a b` = 1; [\"entry\"] = List(2); [false] = Map(1, 2) }"
        })
    );
}

#[test]
fn map_text_recovers_compound_keys() {
    assert_eq!(
        eval(
            r#"
class Bird { name = "owl" }
a = Map(List(1), "a", new Bird {}, "b", Int, "c", Regex("x"), "d").toString()
b = new Mapping { [List(1)] = 2 }.toString()
"#
        ),
        serde_json::json!({
            "a": "Map(List(1), \"a\", new test#Bird { name = \"owl\" }, \"b\", Int, \"c\", Regex(\"x\"), \"d\")",
            "b": "new Mapping { [List(1)] = 2 }"
        })
    );
}

#[test]
fn map_mapping_and_dynamic_have_distinct_classes_and_equality() {
    assert_eq!(
        eval(
            r#"
a = Map().getClass().simpleName
b = Map().getClass() == Map
c = Map().toDynamic().getClass() == Dynamic
d = Map() == new Dynamic {}
e = Map() == new Mapping {}
f = Map("x", 1) == new Dynamic { x = 1 }
g = Map("x", 1) == new Mapping { ["x"] = 1 }
h = Map().toDynamic() == new Dynamic {}
i = Map("x", 1).toMapping() == new Mapping { ["x"] = 1 }
j = List(Map()) == List(new Dynamic {})
k = import("pkl:base").getClass().simpleName
"#
        ),
        serde_json::json!({
            "a": "Map", "b": true, "c": true, "d": false, "e": false,
            "f": false, "g": false, "h": true, "i": true, "j": false,
            "k": "ModuleClass"
        })
    );
}

#[test]
fn map_values_receives_original_keys() {
    assert_eq!(
        eval(
            r#"
local key = new Dynamic { x = 1 }
result = Map(1, "a", 1.5, "b", true, "c", null, "d", 2.s, "e", List(1), "f", key, "g")
  .mapValues((k,v) -> k.toString()).values.toList()
"#
        ),
        serde_json::json!({"result": [
            "1", "1.5", "true", "null", "2.s", "List(1)", "new Dynamic { x = 1 }"
        ]})
    );
}

#[test]
fn map_filter_receives_original_keys() {
    assert_eq!(
        eval(
            r#"
local map = Map(1, "one", true, "yes", null, "nil", List(1), "list")
ints = map.filter((k,v) -> k is Int).toString()
nulls = map.filter((k,v) -> k == null).toString()
lists = map.filter((k,v) -> k is List).toString()
"#
        ),
        serde_json::json!({
            "ints": "Map(1, \"one\")", "nulls": "Map(null, \"nil\")",
            "lists": "Map(List(1), \"list\")"
        })
    );
}

#[test]
fn a_map_key_named_to_string_does_not_override_the_method() {
    assert_eq!(
        eval(r#"result = Map("toString", () -> "oops").toString()"#),
        serde_json::json!({"result": "Map(\"toString\", new Function0 {})"})
    );
}

#[test]
fn contains_key_uses_pkl_value_equality() {
    assert_eq!(
        eval(
            r#"
local map = Map(1, "one", true, "yes", null, "nil", 1.s, "time", List(1), "list", "", "empty")
result = List(map.containsKey(1), map.containsKey(1.0), map.containsKey(true),
  map.containsKey(null), map.containsKey(1000.ms), map.containsKey(List(1)),
  map.containsKey(""), map.containsKey("1"), map.containsKey(false), map.containsKey(2))
"#
        ),
        serde_json::json!({"result": [true, false, true, true, true, true, true, false, false, false]})
    );
}

#[test]
fn imported_modules_retain_their_class_through_map_conversions() {
    let temp = TestTempDir::new("pklr_map_module_identity");
    std::fs::write(
        temp.path().join("lib.pkl"),
        "module birds\nclass Bird {}\nvalue = 1\n",
    )
    .unwrap();
    std::fs::write(
        temp.path().join("other.pkl"),
        "module birds\nclass Bird {}\nvalue = 1\n",
    )
    .unwrap();
    std::fs::write(
        temp.path().join("main.pkl"),
        r#"
import "lib.pkl"
import "other.pkl"
name = lib.getClass().simpleName
text = lib.getClass().toString()
mapClass = lib.toMap().getClass() == Map
dynamicClass = lib.toDynamic().getClass() == Dynamic
rendered = new PcfRenderer {}.renderValue(lib)
valueText = lib.toString()
differentClasses = lib.getClass() == other.getClass()
differentModules = lib == other
sameModule = lib == import("lib.pkl")
"#,
    )
    .unwrap();
    assert_eq!(
        pklr::eval_to_json(&temp.path().join("main.pkl")).unwrap(),
        serde_json::json!({
            "name": "ModuleClass", "text": "birds", "mapClass": true,
            "dynamicClass": true, "rendered": "{\n  value = 1\n}",
            "valueText": "new birds { value = 1 }",
            "differentClasses": false, "differentModules": false, "sameModule": true
        })
    );
}

#[test]
fn module_amendments_retain_their_base_class_but_extensions_declare_a_class() {
    let temp = TestTempDir::new("pklr_map_module_inheritance");
    for (name, source) in [
        (
            "base.pkl",
            "open module birds\nvalue = 1\nselfClass = (() -> module.getClass().toString()).apply()\n",
        ),
        ("same.pkl", "amends \"base.pkl\"\n"),
        ("amend.pkl", "amends \"base.pkl\"\nvalue = 2\n"),
        (
            "child.pkl",
            "module chick\nextends \"base.pkl\"\nimport \"base.pkl\" as Base\nselfIsBase = module is Base\n",
        ),
        (
            "main.pkl",
            r#"
import "base.pkl"
import "same.pkl"
import "amend.pkl"
import "child.pkl"
classes = List(base.getClass() == same.getClass(), base.getClass() == amend.getClass(), base.getClass() == child.getClass())
values = List(base == same, base == amend, base == child)
names = List(base.selfClass, same.selfClass, amend.selfClass, child.selfClass)
types = List(child is base, amend is base, base is child, child.selfIsBase)
"#,
        ),
    ] {
        std::fs::write(temp.path().join(name), source).unwrap();
    }
    assert_eq!(
        pklr::eval_to_json(&temp.path().join("main.pkl")).unwrap(),
        serde_json::json!({
            "classes": [true, true, false], "values": [true, false, false],
            "names": ["birds", "birds", "birds", "chick"], "types": [true, true, false, true]
        })
    );
}

#[test]
fn mapping_defaults_bind_module_types_in_the_declaring_scope() {
    let temp = TestTempDir::new("pklr_map_declared_module_defaults");
    for (name, source) in [
        ("Job.pkl", "module birds.Job\nvalue: Int = 0\n"),
        (
            "Workflow.pkl",
            "module birds.Workflow\nimport \"Job.pkl\" as JobModule\ntypealias Job = *JobModule | Dynamic\njobs: Mapping<String, Job>\n",
        ),
        (
            "main.pkl",
            r#"
amends "Workflow.pkl"
local JobModule = "shadow"
jobs { ["build"] { value = 1 } }
output { value = new Dynamic { data = jobs; jobClass = jobs["build"].getClass().simpleName } }
"#,
        ),
    ] {
        std::fs::write(temp.path().join(name), source).unwrap();
    }
    assert_eq!(
        pklr::eval_to_json(&temp.path().join("main.pkl")).unwrap(),
        serde_json::json!({"data": {"build": {"value": 1}}, "jobClass": "ModuleClass"})
    );
}

#[test]
fn imported_modules_can_type_direct_objects_and_listing_elements() {
    let temp = TestTempDir::new("pklr_map_imported_module_types");
    std::fs::write(temp.path().join("lib.pkl"), "value: Int = 1\n").unwrap();
    std::fs::write(
        temp.path().join("main.pkl"),
        r#"
import "lib.pkl"
result: lib = new { value = 2 }
items: Listing<lib> = new { result }
matches = result is lib
className = result.getClass().simpleName
"#,
    )
    .unwrap();
    assert_eq!(
        pklr::eval_to_json(&temp.path().join("main.pkl")).unwrap(),
        serde_json::json!({
            "result": {"value": 2}, "items": [{"value": 2}], "matches": true,
            "className": "ModuleClass"
        })
    );
}
