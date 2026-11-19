//! Module snapshots and hidden members retain their language identity.

use super::*;
use pretty_assertions::assert_eq;

fn check(name: &str) {
    let cases: serde_json::Value =
        serde_json::from_str(include_str!("../fixtures/module_capture/cases.json")).unwrap();
    let case = cases
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == name)
        .unwrap();
    let temp = TestTempDir::new("pklr_module_capture");
    for (file, source) in case["files"].as_object().unwrap() {
        std::fs::write(temp.path().join(file), source.as_str().unwrap()).unwrap();
    }
    let value = pklr::eval_to_json(&temp.path().join("test.pkl")).unwrap();
    assert_eq!(value, case["expected"], "{name}");
}

#[test]
fn local_list_snapshot() {
    check("local-list-snapshot");
}

#[test]
fn typed_local_list_snapshot() {
    check("typed-local-list-snapshot");
}

#[test]
fn nested_collection_snapshot() {
    check("nested-collection-snapshot");
}

#[test]
fn map_value_snapshot() {
    check("map-value-snapshot");
}

#[test]
fn conditional_module_snapshot() {
    check("conditional-module-snapshot");
}

#[test]
fn local_function_collection_snapshot() {
    check("local-function-collection-snapshot");
}

#[test]
fn local_function_module_result() {
    check("local-function-module-result");
}

#[test]
fn collection_module_identity() {
    check("collection-module-identity");
}

#[test]
fn shadowed_module_map() {
    check("shadowed-module-map");
}

#[test]
fn nested_object_this_is_not_module() {
    check("nested-object-this-is-not-module");
}

#[test]
fn imported_generic_alias_module_snapshot() {
    check("imported-generic-alias-module-snapshot");
}

#[test]
fn hidden_import_members() {
    check("hidden-import-members");
}

#[test]
fn hidden_inherited_members() {
    check("hidden-inherited-members");
}

#[test]
fn hidden_inherited_module_function() {
    check("hidden-inherited-module-function");
}

#[test]
fn hidden_inherited_outer_module() {
    check("hidden-inherited-outer-module");
}

#[test]
fn function_module_parameter_shadowing() {
    check("function-module-parameter-shadowing");
}

#[test]
fn outer_module_identity() {
    check("outer-module-identity");
}

#[test]
fn amended_module_retains_evaluated_members() {
    check("amended-module-retains-evaluated-members");
}
