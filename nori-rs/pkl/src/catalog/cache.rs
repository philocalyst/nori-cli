//! Reading modules from disk, and remembering what each file evaluated to.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::SystemTime;

use regex::Regex;
use serde::de::DeserializeOwned;

use super::Catalog;
use super::Kind;
use super::Loaded;
use super::Problem;
use super::Roots;
use super::Scope;
use crate::EvaluatorOptions;
use crate::kinds::GateDef;
use crate::kinds::JobDef;
use crate::kinds::MonitorDef;
use crate::kinds::PluginDef;
use crate::kinds::SkillMeta;

/// The directory of `$NORI_HOME` holding the modules Nori ships.
const BUILTIN_DIR: &str = "builtin";

/// What a module file decoded to.
#[derive(Debug, Clone, PartialEq)]
enum Parsed {
    Job(JobDef),
    Monitor(MonitorDef),
    Skill(SkillMeta),
    Plugin(PluginDef),
    Gate(GateDef),
}

impl Parsed {
    fn name(&self) -> &str {
        match self {
            Parsed::Job(value) => &value.name,
            Parsed::Monitor(value) => &value.name,
            Parsed::Skill(value) => &value.name,
            Parsed::Plugin(value) => &value.name,
            Parsed::Gate(value) => &value.name,
        }
    }
}

/// What identifies a file's content cheaply.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    modified: SystemTime,
    len: i64,
}

#[derive(Debug)]
struct Entry {
    stamp: Stamp,
    /// The value of the last evaluation that worked, kept while the file is broken.
    good: Option<Parsed>,
    /// Why the current content did not load.
    error: Option<String>,
}

/// Remembers each module file's evaluation, keyed by (path, mtime, length), so a reload
/// re-evaluates only files that changed.
#[derive(Debug, Default)]
pub struct CatalogCache {
    entries: BTreeMap<PathBuf, Entry>,
    last: Option<Catalog>,
    evaluations: i64,
}

static AMENDS: LazyLock<Regex> = LazyLock::new(|| {
    // A constant pattern that compiles; a failure here is a bug every test would hit.
    Regex::new(r#"(?m)^\s*amends\s+"([^"]*)""#).unwrap_or_else(|error| panic!("{error}"))
});

impl CatalogCache {
    /// How many module files this cache has evaluated so far.
    pub fn evaluations(&self) -> i64 {
        self.evaluations
    }

    /// Reads the roots again, evaluating only files whose (path, mtime, length) changed, and
    /// returns the catalog with whether it differs from the previous one (always true the first
    /// time). Blocking. The runtime schemas are added to `options`.
    pub fn reload(&mut self, roots: &Roots, options: &EvaluatorOptions) -> (Catalog, bool) {
        let options = options.clone().with_runtime_schemas();
        let mut old = std::mem::take(&mut self.entries);
        let mut catalog = Catalog::default();
        for kind in Kind::ALL {
            let mut merged: Vec<Loaded<Parsed>> = Vec::new();
            let builtin = roots.user.as_ref().map(|user| user.join(BUILTIN_DIR));
            for (scope, root) in [
                (Scope::Builtin, &builtin),
                (Scope::User, &roots.user),
                (Scope::Project, &roots.project),
            ] {
                let Some(root) = root else { continue };
                let dir = root.join(kind.dir());
                let files = match module_files(&dir) {
                    Ok(files) => files,
                    Err(error) => {
                        catalog.problems.push(Problem {
                            path: dir.clone(),
                            message: format!("cannot read {}: {error}", dir.display()),
                        });
                        continue;
                    }
                };
                let mut names: BTreeMap<String, PathBuf> = BTreeMap::new();
                for (path, stamp) in files {
                    let entry = match old.remove(&path) {
                        Some(entry) if entry.stamp == stamp => entry,
                        previous => {
                            self.evaluations += 1;
                            let good = previous.and_then(|entry| entry.good);
                            match evaluate_module(kind, &path, &options) {
                                Ok(parsed) => Entry {
                                    stamp,
                                    good: Some(parsed),
                                    error: None,
                                },
                                Err(message) => Entry {
                                    stamp,
                                    good,
                                    error: Some(message),
                                },
                            }
                        }
                    };
                    if let Some(message) = &entry.error {
                        catalog.problems.push(Problem {
                            path: path.clone(),
                            message: message.clone(),
                        });
                    }
                    if let Some(parsed) = &entry.good {
                        let name = parsed.name().to_owned();
                        if let Some(first) = names.get(&name) {
                            catalog.problems.push(Problem {
                                path: path.clone(),
                                message: format!(
                                    "{}/{}: name {name:?} is already defined by {}",
                                    kind.dir(),
                                    file_name(&path),
                                    first.display()
                                ),
                            });
                        } else {
                            names.insert(name.clone(), path.clone());
                            merged.push(Loaded {
                                name,
                                path: path.clone(),
                                scope,
                                value: parsed.clone(),
                            });
                        }
                    }
                    self.entries.insert(path, entry);
                }
            }
            // A module shadows the ones of the same name in a root below it.
            let present: Vec<(String, Scope)> = merged
                .iter()
                .map(|loaded| (loaded.name.clone(), loaded.scope))
                .collect();
            merged.retain(|loaded| {
                let above: &[Scope] = match loaded.scope {
                    Scope::Builtin => &[Scope::User, Scope::Project],
                    Scope::User => &[Scope::Project],
                    Scope::Project => &[],
                };
                !present
                    .iter()
                    .any(|(name, scope)| *name == loaded.name && above.contains(scope))
            });
            merged.sort_by(|a, b| a.name.cmp(&b.name));
            for loaded in merged {
                let Loaded {
                    name,
                    path,
                    scope,
                    value,
                } = loaded;
                match value {
                    Parsed::Job(value) => catalog.jobs.push(Loaded {
                        name,
                        path,
                        scope,
                        value,
                    }),
                    Parsed::Monitor(value) => catalog.monitors.push(Loaded {
                        name,
                        path,
                        scope,
                        value,
                    }),
                    Parsed::Skill(value) => catalog.skills.push(Loaded {
                        name,
                        path,
                        scope,
                        value,
                    }),
                    Parsed::Plugin(value) => catalog.plugins.push(Loaded {
                        name,
                        path,
                        scope,
                        value,
                    }),
                    Parsed::Gate(value) => catalog.gates.push(Loaded {
                        name,
                        path,
                        scope,
                        value,
                    }),
                }
            }
        }
        let changed = self.last.as_ref() != Some(&catalog);
        self.last = Some(catalog.clone());
        (catalog, changed)
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The `*.pkl` files directly in `dir`, sorted by file name, with their stamps. A missing
/// directory has none.
fn module_files(dir: &Path) -> std::io::Result<Vec<(PathBuf, Stamp)>> {
    let reader = match std::fs::read_dir(dir) {
        Ok(reader) => reader,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut files = Vec::new();
    for entry in reader {
        let path = entry?.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("pkl") {
            continue;
        }
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let len = i64::try_from(metadata.len()).unwrap_or(i64::MAX);
        files.push((path, Stamp { modified, len }));
    }
    files.sort_by(|a, b| a.0.file_name().cmp(&b.0.file_name()));
    Ok(files)
}

fn decode<T: DeserializeOwned>(
    value: serde_json::Value,
    validate: impl Fn(&T) -> Result<(), Vec<String>>,
) -> Result<T, String> {
    let decoded: T = serde_json::from_value(value).map_err(|error| error.to_string())?;
    validate(&decoded).map_err(|problems| problems.join("; "))?;
    Ok(decoded)
}

/// Evaluates one module file and decodes it into its kind's type, or says what is wrong.
fn evaluate_module(kind: Kind, path: &Path, options: &EvaluatorOptions) -> Result<Parsed, String> {
    let label = format!("{}/{}", kind.dir(), file_name(path));
    let source = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let must_amend = format!("{label} must amend \"{}\"", kind.schema());
    match AMENDS
        .captures(&source)
        .map(|captures| captures[1].to_owned())
    {
        None => return Err(must_amend),
        Some(target) if target.starts_with("nori:") => {
            let accepted = [kind.schema().to_owned(), format!("{}.pkl", kind.schema())];
            if !accepted.contains(&target) {
                return Err(format!("{must_amend}, not \"{target}\""));
            }
        }
        Some(_) => {}
    }
    let value: serde_json::Value =
        crate::native::evaluate_source_at_path(&source, Some(path), options)?;
    let decoded = match kind {
        Kind::Job => decode(value, JobDef::validate).map(Parsed::Job),
        Kind::Monitor => decode(value, MonitorDef::validate).map(Parsed::Monitor),
        Kind::Skill => decode(value, SkillMeta::validate).map(Parsed::Skill),
        Kind::Plugin => decode(value, PluginDef::validate).map(Parsed::Plugin),
        Kind::Gate => decode(value, GateDef::validate).map(Parsed::Gate),
    };
    decoded.map_err(|message| format!("{}: {message} ({must_amend})", path.display()))
}
