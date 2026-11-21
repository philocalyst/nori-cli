//! A conversation out of a Nori home.

use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::path::Path;

use super::CHILD_BINDING;
use super::PortableError;
use super::bundle::BundleFile;
use super::bundle::BundledGit;
use super::bundle::BundledOrigin;
use super::bundle::BundledProject;
use super::bundle::FileKind;
use super::bundle::Manifest;
use super::bundle::Role;
use super::bundle::SessionBundle;
use super::sessions_dir;
use crate::transcript::PROJECT_METADATA_FILE;
use crate::transcript::TranscriptLoader;

/// A delegated child's transcript, as the parent's journal binds it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(super) struct ChildBinding {
    pub(super) project: String,
    pub(super) session: String,
}

/// The children `journal` binds, by their agent-side id: the latest binding of each.
pub(super) fn child_bindings(journal: &Path) -> BTreeMap<String, ChildBinding> {
    let mut children = BTreeMap::new();
    let _ = nori_conversation::ConversationLog::scan(journal, |record| {
        if let nori_conversation::Event::Command { name, payload } = &record.event
            && let Some(child) = name.strip_prefix(CHILD_BINDING)
            && let Ok(binding) = serde_json::from_str::<ChildBinding>(payload)
        {
            children.insert(child.to_owned(), binding);
        }
        ControlFlow::Continue(())
    });
    children
}

/// Read conversation `conversation_id` from the Nori home `home` into a bundle.
pub async fn export(home: &Path, conversation_id: &str) -> Result<SessionBundle, PortableError> {
    let loader = TranscriptLoader::new(home.to_path_buf());
    let meta = loader
        .find_session_metadata_by_id(conversation_id)
        .await?
        .ok_or_else(|| PortableError::NotFound(conversation_id.to_owned()))?;
    if meta.delegated {
        return Err(PortableError::Child(conversation_id.to_owned()));
    }
    let home = home.to_path_buf();
    let cwd = meta.cwd.clone();
    let project_id = meta.project_id.clone();
    let (files, project, git) = tokio::task::spawn_blocking(move || {
        let mut files = Vec::new();
        let root_dir = sessions_dir(&home, &project_id);
        read_session(&root_dir, &meta.session_id, Role::Root, &mut files)?;
        for binding in child_bindings(&root_dir.join(format!("{}.pkl", meta.session_id))).values() {
            let dir = sessions_dir(&home, &binding.project);
            // A child that never wrote a transcript has nothing to carry.
            let _ = read_session(&dir, &binding.session, Role::Child, &mut files);
        }
        let project = std::fs::read_to_string(root_dir.with_file_name(PROJECT_METADATA_FILE))
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
        let project = BundledProject {
            id: project_id.clone(),
            name: project
                .as_ref()
                .and_then(|project| project["name"].as_str())
                .unwrap_or(&project_id)
                .to_owned(),
            git_remote: project
                .as_ref()
                .and_then(|project| project["git_remote"].as_str())
                .map(str::to_owned),
        };
        let git = nori_config::GitRepo::discover(&meta.cwd).map(|repo| BundledGit {
            branch: repo.branch(),
            head: repo.head_commit(),
        });
        Ok::<_, std::io::Error>((files, project, git))
    })
    .await
    .map_err(std::io::Error::other)??;
    Ok(SessionBundle {
        manifest: Manifest {
            conversation_id: conversation_id.to_owned(),
            project,
            origin: BundledOrigin {
                hostname: super::hostname(),
                machine: nori_machines::global_ready()
                    .await
                    .this_computer()
                    .map(ToString::to_string),
                cwd,
                home: dirs::home_dir(),
            },
            agent: meta.agent,
            git,
            exported_at: super::now_iso8601(),
        },
        files,
    })
}

/// The transcript (required) and journal (when there is one) of one session.
fn read_session(
    dir: &Path,
    session_id: &str,
    role: Role,
    files: &mut Vec<BundleFile>,
) -> std::io::Result<()> {
    let transcript = std::fs::read(dir.join(format!("{session_id}.jsonl")))?;
    files.push(BundleFile {
        role,
        session_id: session_id.to_owned(),
        kind: FileKind::Transcript,
        bytes: transcript,
    });
    if let Ok(journal) = std::fs::read(dir.join(format!("{session_id}.pkl"))) {
        files.push(BundleFile {
            role,
            session_id: session_id.to_owned(),
            kind: FileKind::Journal,
            bytes: journal,
        });
    }
    Ok(())
}
