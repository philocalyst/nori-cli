//! Telling the person, once, when Nori cannot save the conversation, and once when it can again
//! (see `nori_harness::storage_health`). Nothing stops meanwhile: the session goes on and what
//! could not be written is written when it can be.

use nori_harness::storage_health::StorageHealth;

use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::history_cell;

/// How often the free space where Nori writes is looked at.
const SPACE_CHECK: std::time::Duration = std::time::Duration::from_secs(300);

/// Follow the state of Nori's writes for the life of the app, and the free space in
/// `nori_home`, so a disk that is filling up is said before anything fails.
pub(crate) fn follow(tx: AppEventSender, nori_home: std::path::PathBuf) {
    let mut health = nori_harness::storage_health::subscribe();
    tokio::spawn(async move {
        let mut space = tokio::time::interval(SPACE_CHECK);
        loop {
            tokio::select! {
                _ = space.tick() => {
                    let home = nori_home.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        nori_harness::storage_health::check_space(&home);
                    })
                    .await;
                    continue;
                }
                changed = health.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
            }
            let state = health.borrow_and_update().clone();
            let cell = match state {
                StorageHealth::Low { free_bytes } => history_cell::new_info_event(
                    format!(
                        "The disk holding Nori's files is nearly full ({} MB left).",
                        free_bytes / (1024 * 1024)
                    ),
                    Some(
                        "If it fills, Nori keeps going and saves what is pending once there is space."
                            .to_owned(),
                    ),
                ),
                StorageHealth::Failing {
                    what, error, full, ..
                } => {
                    let message = if full {
                        "The disk is full: this conversation is not being saved right now."
                            .to_owned()
                    } else {
                        format!("Nori cannot write {what} right now.")
                    };
                    history_cell::new_error_event(format!(
                        "{message} Nothing stops; what is pending is saved, in order, once writing works again. ({error})"
                    ))
                }
                StorageHealth::Ok => {
                    history_cell::new_info_event("Saving again: nothing was lost.".to_owned(), None)
                }
            };
            tx.send(AppEvent::InsertHistoryCell(Box::new(cell)));
        }
    });
}
