//! Tests for pkl language features, organized by category.
//!
//! Tests marked `#[ignore]` document features not yet implemented.
//! As features are added, remove the `#[ignore]` attribute.

use pklr::eval::Evaluator;

fn eval(src: &str) -> serde_json::Value {
    let mut ev = Evaluator::new();
    let path = std::path::Path::new("test.pkl");
    let val = ev.eval_source(src, path).unwrap();
    val.to_json()
}

/// Like eval(), but also applies output.renderer.converters (full pipeline).
fn eval_with_converters(src: &str) -> serde_json::Value {
    let mut ev = Evaluator::new();
    let path = std::path::Path::new("test.pkl");
    let val = ev.eval_source(src, path).unwrap();
    let val = ev.apply_converters(val).unwrap();
    val.to_json()
}

fn eval_fails(src: &str) -> String {
    let mut ev = Evaluator::new();
    let path = std::path::Path::new("test.pkl");
    match ev.eval_source(src, path) {
        Err(e) => e.to_string(),
        Ok(v) => panic!("expected error, got: {:?}", v.to_json()),
    }
}

static NEXT_TEST_DIRECTORY: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

struct TestTempDir {
    path: std::path::PathBuf,
}

impl TestTempDir {
    fn new(name: &str) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("{name}_{}_{unique}_{sequence}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        // macOS exposes temporary files through both /var and /private/var.
        // Compare allowlists and module identities using one physical spelling.
        let path = std::fs::canonicalize(path).unwrap();
        Self { path }
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TestTempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[path = "pkl_features/advanced.rs"]
mod advanced;
#[path = "pkl_features/collections.rs"]
mod collections;
#[path = "pkl_features/constraints.rs"]
mod constraints;
#[path = "pkl_features/dynamic.rs"]
mod dynamic;
#[path = "pkl_features/get_class.rs"]
mod get_class;
#[path = "pkl_features/imports.rs"]
mod imports;
#[path = "pkl_features/listing_default.rs"]
mod listing_default;
#[path = "pkl_features/member_predicates.rs"]
mod member_predicates;
#[path = "pkl_features/null_defaults.rs"]
mod null_defaults;
#[path = "pkl_features/null_safe.rs"]
mod null_safe;
#[path = "pkl_features/numbers.rs"]
mod numbers;
#[path = "pkl_features/objects.rs"]
mod objects;
#[path = "pkl_features/quoted_types.rs"]
mod quoted_types;
#[path = "pkl_features/reference.rs"]
mod reference;
#[path = "pkl_features/reflect.rs"]
mod reflect;
#[path = "pkl_features/render.rs"]
mod render;
#[path = "pkl_features/strictness.rs"]
mod strictness;
#[path = "pkl_features/strings.rs"]
mod strings;
#[path = "pkl_features/syntax.rs"]
mod syntax;
#[path = "pkl_features/typed_elements.rs"]
mod typed_elements;
#[path = "pkl_features/value_presentation.rs"]
mod value_presentation;

#[path = "pkl_features/map_identity.rs"]
mod map_identity;

#[path = "pkl_features/regex_quoting.rs"]
mod regex_quoting;

#[path = "pkl_features/module_capture.rs"]
mod module_capture;
