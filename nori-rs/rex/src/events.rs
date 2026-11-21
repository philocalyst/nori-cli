//! Following a pane's events.

use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::RexPane;

/// What Nori hears from its pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneEvent {
    /// Events of the pane reach Nori (again).
    Following,
    /// The user clicked a link a program printed in the pane, exactly as it was written.
    LinkClicked { url: String },
    /// Events stopped arriving; Nori retries after `retry_in`.
    Lost { reason: String, retry_in: Duration },
}

/// Longest wait between attempts.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Follow `pane`'s events until it closes or the receiver is dropped.
pub fn subscribe(pane: RexPane) -> mpsc::Receiver<PaneEvent> {
    let (tx, rx) = mpsc::channel(32);
    tokio::spawn(async move {
        let mut backoff = Duration::from_millis(500);
        loop {
            let outcome = follow(&pane, &tx).await;
            let reason = match outcome {
                Outcome::ReceiverGone | Outcome::PaneClosed => return,
                Outcome::Ended { reason, followed } => {
                    if followed {
                        backoff = Duration::from_millis(500);
                    }
                    reason
                }
            };
            let lost = PaneEvent::Lost {
                reason,
                retry_in: backoff,
            };
            if tx.send(lost).await.is_err() {
                return;
            }
            tokio::select! {
                () = tx.closed() => return,
                () = tokio::time::sleep(backoff) => {}
            }
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    });
    rx
}

enum Outcome {
    ReceiverGone,
    PaneClosed,
    Ended { reason: String, followed: bool },
}

/// One run of `rex events`.
async fn follow(pane: &RexPane, tx: &mpsc::Sender<PaneEvent>) -> Outcome {
    let child = Command::new(&pane.rex)
        .args(["events", "-s", &pane.session, "--json", "--autostart=false"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(error) => {
            return Outcome::Ended {
                reason: format!("could not start {}: {error}", pane.rex.display()),
                followed: false,
            };
        }
    };
    let Some(stdout) = child.stdout.take() else {
        return Outcome::Ended {
            reason: "rex events gave no output".to_owned(),
            followed: false,
        };
    };
    let mut lines = BufReader::new(stdout).lines();
    let mut followed = false;
    loop {
        // `rex events` prints nothing until something happens; once it has run a moment it is
        // following.
        let next = tokio::select! {
            () = tx.closed() => return Outcome::ReceiverGone,
            next = async {
                if followed {
                    Some(lines.next_line().await)
                } else {
                    tokio::time::timeout(Duration::from_millis(400), lines.next_line()).await.ok()
                }
            } => match next {
                Some(next) => next,
                None => {
                    followed = true;
                    if tx.send(PaneEvent::Following).await.is_err() {
                        return Outcome::ReceiverGone;
                    }
                    continue;
                }
            },
        };
        let line = match next {
            Ok(Some(line)) => line,
            Ok(None) | Err(_) => break,
        };
        if !followed {
            followed = true;
            if tx.send(PaneEvent::Following).await.is_err() {
                return Outcome::ReceiverGone;
            }
        }
        match read(&line, pane) {
            Some(Heard::Event(event)) => {
                if tx.send(event).await.is_err() {
                    return Outcome::ReceiverGone;
                }
            }
            Some(Heard::Closed) => return Outcome::PaneClosed,
            None => {}
        }
    }
    let status = child.wait().await;
    Outcome::Ended {
        reason: match status {
            Ok(status) => format!("rex events ended ({status})"),
            Err(error) => format!("rex events failed: {error}"),
        },
        followed,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Heard {
    Event(PaneEvent),
    /// The pane or its session is gone: nothing more will come.
    Closed,
}

#[derive(Deserialize)]
struct Envelope {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    payload: serde_json::Value,
}

#[derive(Deserialize)]
struct SessionEvent {
    session_id: String,
    event: Envelope,
}

#[derive(Deserialize)]
struct BlockEvent {
    block_id: String,
    name: String,
    #[serde(default)]
    payload: serde_json::Value,
}

/// What one line of `rex events --json` means for `pane`.
fn read(line: &str, pane: &RexPane) -> Option<Heard> {
    let outer: Envelope = serde_json::from_str(line).ok()?;
    match outer.kind.as_str() {
        "session_event" => {}
        "session_destroyed" => {
            let destroyed = outer.payload.get("session_id")?.as_str()?;
            return (destroyed == pane.session).then_some(Heard::Closed);
        }
        _ => return None,
    }
    let session: SessionEvent = serde_json::from_value(outer.payload).ok()?;
    if session.session_id != pane.session {
        return None;
    }
    match session.event.kind.as_str() {
        "block_event" => {
            let block: BlockEvent = serde_json::from_value(session.event.payload).ok()?;
            if block.block_id != pane.block {
                return None;
            }
            match block.name.as_str() {
                "url_clicked" => {
                    let url = block.payload.get("url")?.as_str()?.to_owned();
                    Some(Heard::Event(PaneEvent::LinkClicked { url }))
                }
                "child_exited" => None,
                _ => None,
            }
        }
        "block_closed" => {
            let closed = session.event.payload.get("block_id")?.as_str()?;
            (closed == pane.block).then_some(Heard::Closed)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn pane() -> RexPane {
        RexPane {
            rex: "rex".into(),
            session: "session:s".to_owned(),
            block: "block:b".to_owned(),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_receiver_stops_a_silent_subscription() {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!("nori-rex-test-{}", std::process::id()));
        std::fs::write(&path, "#!/bin/sh\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let pane = RexPane {
            rex: path.clone(),
            ..pane()
        };
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let result = tokio::time::timeout(Duration::from_secs(2), follow(&pane, &tx)).await;
        std::fs::remove_file(path).unwrap();
        assert!(matches!(result, Ok(Outcome::ReceiverGone)));
    }

    #[test]
    fn clicks_on_this_panes_links_are_heard_and_others_are_not() {
        // As `rex events -s SESSION --json` printed it for a click on a link.
        let click = r#"{"type":"session_event","payload":{"session_id":"session:s","event":{"type":"block_event","payload":{"block_id":"block:b","creator_name":"com.superlogical.terminal","name":"url_clicked","payload":{"url":"nori://tok/subagent/3"}}}}}"#;
        assert_eq!(
            read(click, &pane()),
            Some(Heard::Event(PaneEvent::LinkClicked {
                url: "nori://tok/subagent/3".to_owned()
            }))
        );
        let elsewhere = click.replace("block:b", "block:other");
        assert_eq!(read(&elsewhere, &pane()), None);
        let connected = r#"{"type":"session_event","payload":{"session_id":"session:s","event":{"type":"client_connected","payload":{"client_id":"client:c"}}}}"#;
        assert_eq!(read(connected, &pane()), None);
        let closed = r#"{"type":"session_event","payload":{"session_id":"session:s","event":{"type":"block_closed","payload":{"block_id":"block:b"}}}}"#;
        assert_eq!(read(closed, &pane()), Some(Heard::Closed));
    }
}
