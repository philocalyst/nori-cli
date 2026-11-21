//! A bundle into a Nori home.

use std::path::Path;
use std::path::PathBuf;

use super::CHILD_BINDING;
use super::MOVED_IN;
use super::PortableError;
use super::bundle::FileKind;
use super::bundle::Role;
use super::bundle::SessionBundle;
use super::sessions_dir;
use crate::transcript::PROJECT_METADATA_FILE;
use crate::transcript::compute_project_id;

/// Where an imported conversation now lives.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Imported {
    pub conversation_id: String,
    pub project_id: String,
    pub transcript: PathBuf,
    /// How many delegated children came with it.
    pub children: i64,
}

/// Write `bundle` into the Nori home `home`, for the working directory `cwd` here. A conversation
/// that is already here (it moved away and back) is replaced: the bundle is the newer copy.
pub async fn import(
    home: &Path,
    bundle: &SessionBundle,
    cwd: &Path,
) -> Result<Imported, PortableError> {
    if !super::bundle::is_session_id(&bundle.manifest.conversation_id)
        || bundle
            .files
            .iter()
            .any(|file| !super::bundle::is_session_id(&file.session_id))
    {
        return Err(super::bundle::BundleError::Damaged("invalid session id".to_owned()).into());
    }
    let project = compute_project_id(cwd).await?;
    let home = home.to_path_buf();
    let bundle = bundle.clone();
    let cwd = cwd.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let dir = sessions_dir(&home, &project.id);
        std::fs::create_dir_all(&dir)?;
        let project_file = dir.with_file_name(PROJECT_METADATA_FILE);
        if !project_file.exists() {
            let metadata = serde_json::json!({
                "id": project.id,
                "name": project.name,
                "git_remote": project.git_remote,
                "git_root": project.git_root,
                "cwd": project.cwd,
                "created_at": super::now_iso8601(),
                "updated_at": super::now_iso8601(),
            });
            write_atomically(
                &project_file,
                serde_json::to_string_pretty(&metadata)
                    .unwrap_or_default()
                    .as_bytes(),
            )?;
        }
        let mut children = std::collections::BTreeSet::new();
        for file in &bundle.files {
            let path = dir.join(format!("{}.{}", file.session_id, file.kind.extension()));
            let bytes = match file.kind {
                FileKind::Transcript => relocate(&file.bytes, &project.id, &cwd),
                FileKind::Journal => file.bytes.clone(),
            };
            write_atomically(&path, &bytes)?;
            if file.role == Role::Child {
                children.insert(file.session_id.clone());
            }
        }
        let root = bundle.manifest.conversation_id.clone();
        // It moved back here: it is no longer elsewhere.
        let _ = std::fs::remove_file(dir.join(format!("{root}.moved")));
        let journal = dir.join(format!("{root}.pkl"));
        if journal.exists() {
            let mut log = nori_conversation::ConversationLog::open(&journal)?;
            // Children now live beside the root here, whatever project held them there.
            for (child, binding) in super::export::child_bindings(&journal) {
                if binding.project != project.id && children.contains(&binding.session) {
                    log.append(nori_conversation::Event::Command {
                        name: format!("{CHILD_BINDING}{child}"),
                        payload: serde_json::json!({
                            "project": project.id,
                            "session": binding.session,
                        })
                        .to_string(),
                    })?;
                }
            }
            log.append(nori_conversation::Event::Command {
                name: MOVED_IN.to_owned(),
                payload: serde_json::json!({
                    "from": bundle.manifest.origin.hostname,
                    "from_machine": bundle.manifest.origin.machine,
                    "from_cwd": bundle.manifest.origin.cwd,
                    "cwd": cwd,
                    "at": super::now_iso8601(),
                })
                .to_string(),
            })?;
            log.sync()?;
        }
        Ok(Imported {
            transcript: dir.join(format!("{root}.jsonl")),
            conversation_id: root,
            project_id: project.id,
            children: children.len() as i64,
        })
    })
    .await
    .map_err(std::io::Error::other)?
}

/// Write through a temporary file and a rename: a crash leaves the old file or the new one.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let temporary = path.with_extension(format!(
        "{}.incoming",
        path.extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
    ));
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(temporary, path)
}

/// A transcript's first line says where its session lives (project and directory); here, that
/// is this home's project for `cwd`. Where it lived before is in the journal's `moved-in`.
fn relocate(transcript: &[u8], project_id: &str, cwd: &Path) -> Vec<u8> {
    let end = transcript
        .iter()
        .position(|&byte| byte == b'\n')
        .unwrap_or(transcript.len());
    let Ok(mut meta) = serde_json::from_slice::<serde_json::Value>(&transcript[..end]) else {
        return transcript.to_vec();
    };
    if meta["type"] != "session_meta" {
        return transcript.to_vec();
    }
    meta["project_id"] = serde_json::Value::String(project_id.to_owned());
    meta["cwd"] = serde_json::Value::String(cwd.display().to_string());
    let mut out = serde_json::to_vec(&meta).unwrap_or_default();
    out.extend_from_slice(&transcript[end..]);
    out
}
