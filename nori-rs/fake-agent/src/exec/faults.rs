//! Faults and composition: everything that is about the connection, the turn's lifetime, or
//! several things happening at once.

use std::sync::Arc;
use std::time::Duration;

use nori_acp_kit::Sink;
use nori_acp_kit::schema::SessionId;
use serde_json::json;

use super::Break;
use super::Ctx;
use super::Lane;
use super::Run;
use crate::wire::ResponseFault;
use crate::world::Crash;
use crate::world::Detach;
use crate::world::Lane as LaneStep;
use crate::world::Par;
use crate::world::PeerMessage;
use crate::world::RawUpdate;
use crate::world::Repeat;
use crate::world::RespondTwice;
use crate::world::StaleId;
use crate::world::StaleResponse;
use crate::world::StderrFlood;
use crate::world::Throttle;
use crate::world::WireNoise;

/// Real time the tap gets to deliver what the agent already sent, before a fault that closes or
/// disturbs the connection. Not world time: the transport is real even when the clock is not.
const FLUSH: Duration = Duration::from_millis(5);

impl Ctx {
    fn arm(&self, fault: ResponseFault, what: &str) -> Run {
        if !self.env.wire.arm(self.session_id(), fault) {
            self.diverge(format!(
                "{what} needs the wire tap, and this agent is served without one"
            ));
        }
        Ok(())
    }

    pub(super) fn drop_response(&self) -> Run {
        self.arm(ResponseFault::Drop, "drop_response")
    }

    pub(super) fn respond_twice(&self, twice: &RespondTwice) -> Run {
        self.arm(
            ResponseFault::Twice {
                gap: twice.gap.0,
                second_stop_reason: twice
                    .second_stop_reason
                    .and_then(|reason| serde_json::to_value(reason).ok())
                    .and_then(|reason| reason.as_str().map(str::to_owned)),
            },
            "respond_twice",
        )
    }

    pub(super) fn stale_response(&self, stale: &StaleResponse) -> Run {
        self.arm(
            ResponseFault::Stale {
                previous: stale.id == StaleId::Previous,
                gap: stale.gap.0,
            },
            "stale_response",
        )
    }

    /// The turn goes on without the turn: the steps run on a task of their own, which no cancel
    /// reaches and which only the end of the connection stops.
    pub(super) fn detach(&self, detach: &Detach) -> Run {
        let tail = Self {
            cancel: None,
            participant: Arc::new(self.participant.join()),
            ..self.clone()
        };
        let steps = detach.steps.clone();
        let after = detach.after.0;
        tokio::spawn(async move {
            if !after.is_zero() && tail.sleep(after).await.is_err() {
                return;
            }
            let _ = tail.run(&steps).await;
        });
        Ok(())
    }

    pub(super) async fn crash(&self, crash: &Crash) -> Run {
        tokio::time::sleep(FLUSH).await;
        if !self.env.wire.crash(
            crash.exit_code,
            crash.message.clone(),
            crash.mode == crate::world::CrashMode::Sigkill,
        ) {
            self.diverge("crash needs the wire tap, and this agent is served without one");
            return Ok(());
        }
        Err(Break::Crashed)
    }

    pub(super) async fn wire_noise(&self, noise: &WireNoise) -> Run {
        tokio::time::sleep(FLUSH).await;
        if !self.env.wire.noise(noise.line.clone(), noise.newline) {
            self.diverge("wire_noise needs the wire tap, and this agent is served without one");
        }
        Ok(())
    }

    pub(super) async fn close_stdout(&self) -> Run {
        tokio::time::sleep(FLUSH).await;
        if !self.env.wire.close_stdout() {
            self.diverge("close_stdout needs the wire tap, and this agent is served without one");
        }
        Ok(())
    }

    pub(super) async fn throttle(&self, throttle: &Throttle) -> Run {
        tokio::time::sleep(FLUSH).await;
        if !self.env.wire.throttle(throttle.bytes_per_second) {
            self.diverge("throttle needs the wire tap, and this agent is served without one");
        }
        Ok(())
    }

    pub(super) async fn stderr_flood(&self, flood: &StderrFlood) -> Run {
        if !self.env.wire.is_external() {
            self.diverge(
                "stderr_flood needs a served process: an in-process agent has no stderr of its own",
            );
            return Ok(());
        }
        let line = "x".repeat(usize::try_from(flood.bytes_per_line.max(1)).unwrap_or(1));
        // Blocking writes on purpose: a process that floods stderr blocks when nobody reads it.
        let lines = flood.lines;
        let _ = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let mut stderr = std::io::stderr().lock();
            for index in 0..lines {
                if writeln!(stderr, "flood {index} {line}").is_err() {
                    break;
                }
            }
        })
        .await;
        Ok(())
    }

    /// The session `name` stands for: this one, another of this agent's, or an id taken verbatim.
    fn resolve_lane(&self, name: &str) -> SessionId {
        let sessions = self.env.rt.sessions_of(self.agent());
        let this = self.session_id();
        let found = match name {
            "self" => None,
            "first" => sessions.first().map(|session| session.id.clone()),
            "newest" => sessions.last().map(|session| session.id.clone()),
            "previous" => sessions
                .iter()
                .rev()
                .skip_while(|session| session.id != this)
                .nth(1)
                .map(|session| session.id.clone()),
            other => Some(self.render(other)),
        };
        SessionId::new(found.unwrap_or_else(|| this.to_owned()))
    }

    fn lane_for(&self, id: SessionId) -> Lane {
        let recorded = id == self.lane.id && self.lane.recorded;
        Lane {
            sink: Sink::new(self.lane.sink.cx(), id.clone()),
            id,
            recorded,
        }
    }

    pub(super) fn raw_update(&self, raw: &RawUpdate) -> Run {
        let session = raw
            .lane
            .as_deref()
            .map_or_else(|| self.lane.id.clone(), |name| self.resolve_lane(name));
        self.emit_raw(&session, raw.update.clone())
    }

    pub(super) async fn lane_steps(&self, lane: &LaneStep) -> Run {
        let id = self.resolve_lane(&lane.session);
        let other = Self {
            lane: self.lane_for(id),
            ..self.clone()
        };
        other.run(&lane.steps).await
    }

    /// Lanes at once: each has its own share of the clock, and their updates interleave in the
    /// order the clock wakes them. This task waits for them without holding time back.
    pub(super) async fn par(&self, par: &Par) -> Run {
        let lanes: Vec<_> = par
            .lanes
            .iter()
            .map(|steps| {
                let lane = Self {
                    participant: Arc::new(self.participant.join()),
                    ..self.clone()
                };
                async move { lane.run(steps).await }
            })
            .collect();
        self.participant
            .park(futures::future::join_all(lanes))
            .await
            .into_iter()
            .collect::<Result<Vec<()>, Break>>()
            .map(|_| ())
    }

    /// A `session_message` between two sessions, delivered on the recipient's.
    pub(super) fn peer_message(&self, message: &PeerMessage) -> Run {
        let from = self.resolve_lane(&message.from);
        let to = self.resolve_lane(&message.to);
        let mut update = json!({
            "sessionUpdate": "session_message",
            "senderSessionId": from,
            "recipientSessionId": to,
            "content": {"type": "text", "text": self.text(&message.text)},
        });
        if let Some(id) = &message.id {
            update["messageId"] = json!(self.render(id));
        }
        self.emit_raw(&to, update)
    }

    pub(super) async fn repeat(&self, repeat: &Repeat) -> Run {
        for index in 1..=repeat.times {
            let mut scope = (*self.scope).clone();
            scope.insert("i".to_owned(), index.to_string());
            let inner = Self {
                scope: Arc::new(scope),
                ..self.clone()
            };
            inner.run(&repeat.steps).await?;
            if index < repeat.times && !repeat.every.is_zero() {
                self.sleep(repeat.every.0).await?;
            }
        }
        Ok(())
    }
}
