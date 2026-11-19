//! Loading the sessions `/resume` offers: the recorded transcripts of this project written by
//! the active agent. The picker itself is [`crate::resume_picker`].

use std::path::Path;
use std::time::Instant;

use nori_harness::transcript::TranscriptLoader;

use crate::resume_picker::SessionPickerInfo;

/// Load resumable sessions for the given agent and working directory.
///
/// Filters sessions to only include those created by the specified agent,
/// since `session/load` can only resume sessions from the same agent type.
pub async fn load_resumable_sessions(
    nori_home: &Path,
    cwd: &Path,
    agent_filter: &str,
) -> std::io::Result<Vec<SessionPickerInfo>> {
    let started = Instant::now();
    tracing::info!(
        target: "nori_resume",
        phase = "load_resumable_sessions.start",
        nori_home = %nori_home.display(),
        cwd = %cwd.display(),
        agent_filter = %agent_filter,
        "loading /resume sessions before picker display",
    );

    // Filter by agent before any transcript body work. Different agents have
    // incompatible resume formats, and transcript summary loading may scan
    // large files.
    let filter_started = Instant::now();
    let loader = TranscriptLoader::new(nori_home.to_path_buf());
    let session_infos = loader.find_session_metadata_for_cwd(cwd).await?;
    let session_info_count = session_infos.len();

    let matching_session_infos: Vec<_> = session_infos
        .into_iter()
        .filter(|info| info.agent.as_deref() == Some(agent_filter))
        .collect();

    tracing::info!(
        target: "nori_resume",
        phase = "load_resumable_sessions.agent_filter_metadata_loaded",
        elapsed_ms = filter_started.elapsed().as_millis(),
        total_elapsed_ms = started.elapsed().as_millis(),
        session_info_count,
        matching_session_count = matching_session_infos.len(),
        agent_filter = %agent_filter,
        "loaded session metadata for /resume agent filtering",
    );

    let filtered: Vec<SessionPickerInfo> = matching_session_infos
        .into_iter()
        .map(SessionPickerInfo::from)
        .collect();
    tracing::info!(
        target: "nori_resume",
        phase = "load_resumable_sessions.metadata_rows_built",
        total_elapsed_ms = started.elapsed().as_millis(),
        returned_session_count = filtered.len(),
        agent_filter = %agent_filter,
        "built metadata-only resumable session rows",
    );

    tracing::info!(
        target: "nori_resume",
        phase = "load_resumable_sessions.done",
        total_elapsed_ms = started.elapsed().as_millis(),
        returned_session_count = filtered.len(),
        agent_filter = %agent_filter,
        "finished loading /resume sessions before picker display",
    );

    Ok(filtered)
}
