//! A world's own `NORI_HOME`: configuration seeded from the parent's, credentials left behind.
//!
//! Agents keep their own authentication (in their own homes, outside `NORI_HOME`), so an
//! isolated Nori home needs only Nori's settings. What is copied is an allowlist: `config.toml`
//! with every credential-looking key removed, `init.pkl` and the custom `commands/`. History,
//! sessions, transcripts, logs, `.env` and anything else in the parent's home are not copied,
//! so a world neither sees nor writes the parent's records.

use std::path::Path;

use toml_edit::DocumentMut;
use toml_edit::Item;

/// Key names that may hold a secret, matched case-insensitively as substrings.
const CREDENTIAL_KEYS: [&str; 9] = [
    "token",
    "secret",
    "password",
    "passwd",
    "credential",
    "api_key",
    "apikey",
    "auth",
    "private_key",
];

fn strip_credentials(item: &mut Item) {
    match item {
        Item::Table(table) => {
            let secret: Vec<String> = table
                .iter()
                .filter(|(key, _)| {
                    let key = key.to_lowercase();
                    CREDENTIAL_KEYS.iter().any(|word| key.contains(word))
                })
                .map(|(key, _)| key.to_owned())
                .collect();
            for key in secret {
                table.remove(&key);
            }
            for (_, child) in table.iter_mut() {
                strip_credentials(child);
            }
        }
        Item::ArrayOfTables(tables) => {
            for table in tables.iter_mut() {
                let mut child = Item::Table(std::mem::take(table));
                strip_credentials(&mut child);
                if let Item::Table(cleaned) = child {
                    *table = cleaned;
                }
            }
        }
        Item::Value(value) => {
            if let Some(inline) = value.as_inline_table_mut() {
                let secret: Vec<String> = inline
                    .iter()
                    .filter(|(key, _)| {
                        let key = key.to_lowercase();
                        CREDENTIAL_KEYS.iter().any(|word| key.contains(word))
                    })
                    .map(|(key, _)| key.to_owned())
                    .collect();
                for key in secret {
                    inline.remove(&key);
                }
            }
        }
        Item::None => {}
    }
}

fn copy_plain_files(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_plain_files(&entry.path(), &target)?;
        } else if entry.file_type()?.is_file() {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// Fill `world` with the settings of `parent`. Missing files in the parent are simply absent.
pub(super) fn seed(parent: &Path, world: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(world)?;
    match std::fs::read_to_string(parent.join("config.toml")) {
        Ok(text) => {
            let mut document: DocumentMut = text.parse().map_err(|error| {
                std::io::Error::other(format!("{}: {error}", parent.join("config.toml").display()))
            })?;
            strip_credentials(document.as_item_mut());
            std::fs::write(world.join("config.toml"), document.to_string())?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    match std::fs::copy(parent.join("init.pkl"), world.join("init.pkl")) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    match copy_plain_files(&parent.join("commands"), &world.join("commands")) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(())
}
