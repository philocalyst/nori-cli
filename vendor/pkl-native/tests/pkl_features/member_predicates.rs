use super::{eval, eval_fails};

#[test]
fn listing_predicate_amends_matching_elements() {
    let value = eval(
        r#"
people = new Listing {
  new { name = "Pigeon"; age = 42 }
  new { name = "Barn Owl"; age = 21 }
}
result = (people) {
  [[name == "Barn Owl"]] { age = 99 }
}
"#,
    );
    assert_eq!(value["result"][1]["name"], "Barn Owl");
    assert_eq!(value["result"][1]["age"], 99);
    assert_eq!(value["result"][0]["age"], 42);
}

#[test]
fn mapping_predicate_amends_matching_values() {
    let value = eval(
        r#"
people = new Mapping {
  ["Pigeon"] { name = "Pigeon"; age = 42 }
  ["Barn Owl"] { name = "Barn Owl"; age = 21 }
}
result = (people) {
  [[name == "Barn Owl"]] { age = 99 }
}
"#,
    );
    assert_eq!(value["result"]["Barn Owl"]["age"], 99);
    assert_eq!(value["result"]["Pigeon"]["age"], 42);
}

#[test]
fn predicate_conflicting_override_is_rejected() {
    let error = eval_fails(
        r#"
people = new Listing { new { name = "Pigeon"; age = 42 } }
result = (people) {
  [0] { age = 4 }
  [[name == "Pigeon"]] { age = 9 }
}
"#,
    );
    assert!(error.contains("Duplicate definition"), "{error}");
}
