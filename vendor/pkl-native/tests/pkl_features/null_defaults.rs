//! Pkl null defaults remain invisible until an amendment requests their parent.

use super::*;

#[test]
fn null_defaults_keep_typed_dynamic_listing_mapping_and_function_parents() {
    let actual = eval(
        r#"
class Person { name: String; age: Int = 0 }
local person = Null(new Person { name = "Pigeon" })
local dynamic = Null(new Dynamic { first = 1 })
local listing = Null(new Listing { "one" })
local mapping = Null(new Mapping { ["one"] = 1 })
local fun = Null((name) -> new Dynamic { value = name })
typed = (person) { age = 3 }
object = (dynamic) { second = 2 }
elements = (listing) { "two" }
entries = (mapping) { ["two"] = 2 }
result = ((fun) { extra = 42 }).apply("hello")
"#,
    );
    assert_eq!(
        actual,
        serde_json::json!({
            "typed": {"name": "Pigeon", "age": 3},
            "object": {"first": 1, "second": 2},
            "elements": ["one", "two"],
            "entries": {"one": 1, "two": 2},
            "result": {"value": "hello", "extra": 42}
        })
    );
}

#[test]
fn stored_defaults_are_null_for_operators_collections_and_member_calls() {
    let actual = eval(
        r#"
import "pkl:test"
local n = Null(new Dynamic { value = 1 })
isNull = n is Null
equal = n == null && null == n && n == Null(new Listing {})
coalesced = n ?? "fallback"
safe = n?.value
safeCall = n?.toString()
unsafeField = test.catch(() -> n.value)
unsafeMethod = test.catch(() -> n.foo())
classMatches = n.getClass() == null.getClass()
string = n.toString()
setSize = Set(null, n, Null(new Mapping {})).length
key = Map(n, "stored")[null]
filtered = List(1, 2).mapNonNull((it) -> n)
filteredIndexed = List(1, 2).mapNonNullIndexed((i, it) -> n)
"#,
    );
    assert_eq!(
        actual,
        serde_json::json!({
            "isNull": true, "equal": true, "coalesced": "fallback", "safe": null,
            "safeCall": null,
            "unsafeField": "Cannot find property `value` in object of type `Null`.",
            "unsafeMethod": "Cannot find method `foo` in class `Null`.",
            "classMatches": true, "string": "null", "setSize": 1,
            "key": "stored", "filtered": [], "filteredIndexed": []
        })
    );
}

#[test]
fn literal_null_defaults_stay_lazy_and_capture_the_declaring_scope() {
    let actual = eval(
        r#"
function make(name) = Null(new Dynamic { inherited = name })
local n = make("original")
local name = "other"
result = (n) { added = name }
unused = Null(if (true) new Listing { throw("unused") } else new Listing {})
"#,
    );
    assert_eq!(
        actual,
        serde_json::json!({
            "result": {"inherited": "original", "added": "other"}, "unused": null
        })
    );
}

#[test]
fn property_body_amends_its_inherited_null_default() {
    let actual = eval(
        r#"
class Holder {
  item = Null(new Dynamic { inherited = 1 })
}
result = new Holder { item { added = 2 } }
"#,
    );
    assert_eq!(
        actual,
        serde_json::json!({"result": {"item": {"inherited": 1, "added": 2}}})
    );
}

#[test]
fn null_literal_amendment_starts_from_dynamic() {
    let actual = eval("result = (null) { value = 1 }\n");
    assert_eq!(actual, serde_json::json!({"result": {"value": 1}}));
}

#[test]
fn amendments_keep_default_parent_restrictions() {
    assert!(eval_fails("result = (Null(new Listing {})) { value = 1 }\n").contains("Listing"));
    assert!(eval_fails("result = (Null(new Mapping {})) { 1 }\n").contains("Mapping"));
    assert!(eval_fails("result = (Null(new Mapping {})) { value = 1 }\n").contains("Mapping"));
    assert!(
        eval_fails("class Person { age: Int = 1 }\nresult = (Null(new Person {})) { name = 1 }\n")
            .contains("name")
    );
}

#[test]
fn renderers_treat_stored_defaults_as_null_in_settings_and_members() {
    let actual = eval(
        r#"
import "pkl:jsonnet"
local n = Null(new Dynamic { value = 1 })
compact = new jsonnet.Renderer { indent = n }.renderValue(new Dynamic { a = 1 })
omitted = new JsonRenderer { omitNullProperties = true }.renderValue(new Dynamic { a = n; b = 1 })
kept = new JsonRenderer { omitNullProperties = false; indent = "" }.renderValue(new Dynamic { a = n })
"#,
    );
    assert_eq!(
        actual,
        serde_json::json!({
            "compact": "{ a: 1 }", "omitted": "{\n  \"b\": 1\n}", "kept": "{\"a\":null}"
        })
    );
}

#[test]
fn parameterized_amendments_validate_kind_and_arity_when_constructed() {
    let actual = eval(
        r#"
import "pkl:test"
local f = (x, y) -> new Dynamic {}
wrongArity = test.catch(() -> (Null(f)) { x -> value = x })
wrongKind = test.catch(() -> (Null(new Listing {})) { x -> x })
wrongObject = test.catch(() -> (Null(new Dynamic {})) { x -> for (i in List(2)) { i } })
"#,
    );
    assert_eq!(
        actual,
        serde_json::json!({
            "wrongArity": "Expected a function with 1 parameters, but got 2.",
            "wrongKind": "Expected a function that returns a `Listing`, but got a `Listing`.",
            "wrongObject": "Expected a function that returns an object, but got an object."
        })
    );
}

#[test]
fn nullable_annotations_retain_literal_and_mixin_defaults() {
    let actual = eval(
        r#"
import "pkl:test"
hidden literal: "hello"?
hidden mixin: Mixin<Dynamic>?
error = test.catch(() -> (literal) { value = 1 })
result = ((mixin) { added = 2 }).apply(new Dynamic { inherited = 1 })
"#,
    );
    assert_eq!(
        actual,
        serde_json::json!({
            "error": "Cannot instantiate, or amend an instance of, external class `String`.",
            "result": {"inherited": 1, "added": 2}
        })
    );
}

#[test]
fn ordinary_dynamic_bodies_keep_pkl_parameter_semantics() {
    let actual = eval(
        r#"
local d = Null(new Dynamic { inherited = 1 })
result = (d) { ignored -> added = 2 }
plain = (null) { ignored -> added = 3 }
"#,
    );
    assert_eq!(
        actual,
        serde_json::json!({
            "result": {"inherited": 1, "added": 2}, "plain": {"added": 3}
        })
    );
}

#[test]
fn null_constructor_checks_value_categories_and_inferred_new() {
    for source in [
        "value = Null(Map())",
        "value = Null(Pair(1, 2))",
        "value = Null(new {})",
    ] {
        let error = eval_fails(source);
        assert!(
            error.contains("Object | Function<Object>"),
            "{source}: {error}"
        );
    }
    assert_eq!(
        eval("class Pair {}\nvalue = Null(new Pair {})"),
        serde_json::json!({"value": null})
    );
}

#[test]
fn catch_forces_deferred_collection_member_errors() {
    let actual = eval(
        r#"
import "pkl:test"
first = test.catch(() -> new Listing { throw("forced") })
nested = test.catch(() -> new Mapping { ["value"] = new Listing { throw("nested") } })
nullDefault = test.catchOrNull(() -> Null(new Listing { throw("unused") }))
"#,
    );
    assert_eq!(
        actual,
        serde_json::json!({"first": "forced", "nested": "nested", "nullDefault": null})
    );
}

#[test]
fn nullable_typed_class_and_recursive_listing_expression_defaults_amend_without_recursion() {
    let actual = eval(
        r#"
class Person {
  name: String = "Pigeon"
  age: Int = 0
}
class Node {
  label: String = "root"
  next: Node?
  children: Listing<Node>
}
hidden person: Person?
hidden node: Node?
hidden tasks: Listing<Person>?

amendedPerson = (person) { age = 7 }
amendedTasks = (tasks) { new { age = 9 } }
baseNode = new Node {}
amendedNext = (baseNode.next) { label = "second"; next = (next) { label = "third" } }
amendedChildren = (baseNode.children) {
  new {
    label = "child-1"
    children {
      new { label = "grandchild-1" }
    }
  }
}
amendedNullNode = (node) { label = "from-null-annotation" }
"#,
    );
    assert_eq!(
        actual["amendedPerson"],
        serde_json::json!({"name": "Pigeon", "age": 7})
    );
    assert_eq!(
        actual["amendedTasks"],
        serde_json::json!([{"name": "Pigeon", "age": 9}])
    );
    assert_eq!(
        actual["amendedNext"],
        serde_json::json!({
            "label": "second",
            "next": {"label": "third", "next": null, "children": []},
            "children": []
        })
    );
    assert_eq!(
        actual["amendedChildren"],
        serde_json::json!([{
            "label": "child-1",
            "next": null,
            "children": [{"label": "grandchild-1", "next": null, "children": []}]
        }])
    );
    assert_eq!(
        actual["amendedNullNode"],
        serde_json::json!({"label": "from-null-annotation", "next": null, "children": []})
    );
    assert!(
        eval_fails("class Closed { age: Int = 0 }\nhidden c: Closed?\nx = (c) { nope = 1 }\n")
            .contains("nope")
    );
}

#[test]
fn parentless_anonymous_new_is_rejected_while_typed_and_property_new_succeed() {
    for invalid in [
        "x = List(new { a = 1 })\n",
        "x = Set(new { a = 1 })\n",
        "x = Map(\"k\", new { a = 1 })\n",
        "function f() = new { a = 1 }\nx = f()\n",
        "function f(): Any = new { a = 1 }\nx = f()\n",
        "x: Any = new { a = 1 }\n",
        "f = () -> if (true) new {} else new {}\n",
        "x = (if (true) new {} else new {})\n",
        "x = new { a = 1 }.a\n",
        "x = new { a = 1 } == new { a = 1 }\n",
        "m = new Mapping { [new { a = 1 }] = 1 }\n",
    ] {
        let error = eval_fails(invalid);
        assert!(
            error.contains("Cannot tell which parent to amend"),
            "{invalid}: {error}"
        );
    }
}
