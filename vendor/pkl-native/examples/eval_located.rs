//! Evaluate a module to host data and, on failure, print the error with the properties that
//! were being evaluated: `cargo run --example eval_located -- file.pkl`.

use std::path::Path;

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: eval_located <file.pkl>");
        std::process::exit(2);
    };
    let path = std::fs::canonicalize(Path::new(&path)).unwrap_or_else(|_| path.into());
    let source = std::fs::read_to_string(&path).unwrap_or_default();
    // NORI_SCHEMAS=dir registers every dir/X.pkl as the virtual module `nori:X`.
    let mut builder = pklr::EvaluatorBuilder::new();
    if let Some(dir) = std::env::var_os("NORI_SCHEMAS") {
        let mut modules = std::collections::BTreeMap::new();
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let file = entry.path();
            if let (Some(stem), Ok(text)) = (file.file_stem(), std::fs::read_to_string(&file)) {
                modules.insert(format!("nori:{}", stem.to_string_lossy()), text);
            }
        }
        builder = builder.virtual_modules(modules);
    }
    let mut evaluator = builder.build();
    match evaluator.eval_source_data(&source, &path) {
        Ok(value) => println!("{value}"),
        Err(error) => {
            println!("ERROR: {error}");
            for frame in evaluator.error_frames() {
                let line = frame.source[..frame.declaration.start].matches('\n').count() + 1;
                println!("  frame {} `{}` line {line}", frame.module, frame.property);
            }
            let text = error.to_string();
            let message = text.strip_prefix("Eval error: ").unwrap_or(&text);
            for object in evaluator.error_objects(message) {
                let names: Vec<String> = object
                    .properties
                    .iter()
                    .map(|p| format!("{}@{}", p.property, p.source[..p.declaration.start].matches('\n').count() + 1))
                    .collect();
                println!("  object sets: {}", names.join(", "));
            }
            for step in evaluator.error_module_chain() {
                println!("  chain: {step}");
            }
        }
    }
}
