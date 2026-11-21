//! The OpenCode API key: `OPENCODE_API_KEY` when it is set, else the key Nori keeps for the
//! account (written by a sign-in, removed by a sign-out), readable by its owner only.

use std::path::Path;

use nori_llm::Secret;

use crate::env::Env;

/// The variable that holds a key, and that a sign-in hands the agent the key it was given in.
pub(crate) const VARIABLE: &str = "OPENCODE_API_KEY";
/// The file a kept key is in, in the account's home.
pub(crate) const FILE: &str = "opencode-api-key";

/// Where a key in use comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    Environment,
    Kept,
}

/// The key in use, and where it comes from.
pub(crate) async fn read(env: &Env, file: &Path) -> Option<(Secret, Source)> {
    if let Some(key) = env.get(VARIABLE) {
        return Some((Secret::new(key), Source::Environment));
    }
    let text = tokio::fs::read_to_string(file).await.ok()?;
    let key = text.trim();
    (!key.is_empty()).then(|| (Secret::new(key.to_owned()), Source::Kept))
}

/// Keep `key` for the account, replacing any kept before.
pub(crate) async fn keep(file: &Path, key: &str) -> std::io::Result<()> {
    if let Some(parent) = file.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let temporary = file.with_extension("tmp");
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut written = options.open(&temporary).await?;
    tokio::io::AsyncWriteExt::write_all(&mut written, key.trim().as_bytes()).await?;
    written.sync_all().await?;
    tokio::fs::rename(&temporary, file).await
}

/// Forget the kept key. A key that was never kept is already forgotten.
pub(crate) async fn forget(file: &Path) -> std::io::Result<()> {
    match tokio::fs::remove_file(file).await {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        Ok(()) | Err(_) => Ok(()),
    }
}
