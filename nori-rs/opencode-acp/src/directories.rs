//! Scope OpenCode's external-directory grants to the roots supplied by ACP.

use std::collections::HashSet;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use nori_acp_kit::acp;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;

const METADATA_KEY: &str = "opencode.acp.additionalDirectories";

fn normalize(path: &Path) -> acp::Result<PathBuf> {
    if !path.is_absolute()
        || path.to_string_lossy().contains('*')
        || path.to_string_lossy().contains('?')
    {
        return Err(acp::Error::invalid_params()
            .data("Additional directories must be absolute paths without wildcards"));
    }
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                result.push(component)
            }
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
        }
    }
    Ok(result)
}

fn rules(paths: &[String]) -> acp::Result<Vec<Value>> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for path in paths {
        let original = PathBuf::from(path);
        let canonical = std::fs::canonicalize(&original).unwrap_or_else(|_| original.clone());
        for directory in [original, canonical] {
            let resource = directory
                .join("*")
                .to_str()
                .ok_or_else(acp::Error::invalid_params)?
                .to_owned();
            if seen.insert(resource.clone()) {
                result.push(
                    json!({"action":"external_directory","resource":resource,"effect":"allow"}),
                );
            }
        }
    }
    Ok(result)
}

/// Replace only the grants previously installed by this adapter, preserving other rules.
pub(crate) fn update(cwd: &Path, session: &Value, directories: Vec<PathBuf>) -> acp::Result<Value> {
    let cwd = normalize(cwd)?;
    let resolved_cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    let mut seen = HashSet::new();
    let mut paths = Vec::new();
    for directory in directories {
        let directory = normalize(&directory)?;
        let resolved = std::fs::canonicalize(&directory).unwrap_or_else(|_| directory.clone());
        if resolved == resolved_cwd {
            continue;
        }
        let path = directory
            .to_str()
            .ok_or_else(acp::Error::invalid_params)?
            .to_owned();
        if seen.insert(path.clone()) {
            paths.push(path);
        }
    }
    let mut metadata = session["metadata"]
        .as_object()
        .cloned()
        .unwrap_or_else(Map::new);
    let previous = metadata
        .remove(METADATA_KEY)
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|value| value.as_str().map(ToOwned::to_owned))
        .collect::<Vec<_>>();
    if !paths.is_empty() {
        metadata.insert(METADATA_KEY.to_owned(), json!(paths));
    }
    let owned = rules(&previous)?;
    let mut permissions = rules(&paths)?;
    permissions.extend(
        session["permissions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|rule| !owned.contains(rule))
            .cloned(),
    );
    Ok(json!({"permissions":permissions,"metadata":metadata}))
}

#[cfg(test)]
mod tests {
    use super::update;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::path::Path;
    use std::path::PathBuf;

    #[test]
    fn replaces_only_owned_grants_and_rejects_wildcards() {
        let session = json!({"metadata":{"other":"preserved","opencode.acp.additionalDirectories":["/old"]},"permissions":[{"action":"external_directory","resource":"/old/*","effect":"allow"},{"action":"shell","resource":"*","effect":"ask"}]});
        let result = update(
            Path::new("/work"),
            &session,
            vec![PathBuf::from("/new"), PathBuf::from("/new")],
        )
        .unwrap();
        assert_eq!(
            result,
            json!({"metadata":{"other":"preserved","opencode.acp.additionalDirectories":["/new"]},"permissions":[{"action":"external_directory","resource":"/new/*","effect":"allow"},{"action":"shell","resource":"*","effect":"ask"}]})
        );
        assert!(update(Path::new("/work"), &session, vec![PathBuf::from("/bad/*")]).is_err());
        assert!(
            update(
                Path::new("/work"),
                &session,
                vec![PathBuf::from("relative")]
            )
            .is_err()
        );
    }
}
