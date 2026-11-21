//! What the edit journal says the latest turn changed, for the stop gates.

use std::path::PathBuf;

use super::EditJournal;

impl EditJournal {
    /// The files the latest turn's tool calls reported editing, each once, as the agents named
    /// them. Only what agents reported: a change made by a shell command is not here.
    pub(crate) fn latest_turn_changed_paths(&self) -> Vec<PathBuf> {
        let turns = self.lock();
        let Some(turn) = turns.last() else {
            return Vec::new();
        };
        let mut paths: Vec<PathBuf> = turn
            .calls
            .iter()
            .flat_map(|call| call.edits.iter())
            .map(|edit| edit.path.clone())
            .collect();
        paths.sort_unstable();
        paths.dedup();
        paths
    }
}
