use super::EvaluatorOptions;
use std::path::Path;
use std::path::PathBuf;

pub(super) fn evaluate(
    source: &str,
    path: Option<&Path>,
    options: &EvaluatorOptions,
) -> Result<serde_json::Value, String> {
    let mut builder = pklr::EvaluatorBuilder::new()
        .allowed_modules(options.allowed_modules.clone())
        .allowed_resources(options.allowed_resources.clone())
        .external_properties(options.properties.clone())
        .virtual_modules(options.virtual_modules.clone());
    if let Some(environment) = &options.environment {
        builder = builder.environment_variables(environment.clone());
    }
    let mut evaluator = builder.build();
    let depth = usize::try_from(options.max_depth)
        .ok()
        .filter(|depth| *depth > 0)
        .ok_or_else(|| "Pkl evaluation max_depth must be positive".to_string())?;
    evaluator.set_max_depth(depth);
    let path = match path {
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(path) => std::env::current_dir()
            .map_err(|error| error.to_string())?
            .join(path),
        None => PathBuf::from("repl:text"),
    };
    let file = path.display().to_string();
    evaluator
        .eval_source_data(source, &path)
        .map_err(|error| super::diagnostics::describe(&error, &evaluator, source, &file))
}
