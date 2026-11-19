//! The interpreter: running a scene's steps against a live session.
//!
//! A [`Ctx`] is one task's view of the world: which session it speaks for, how it sleeps, whether
//! a cancel interrupts it. Steps run in order; a step that ends the turn (a stop, an error, a hang)
//! breaks out with a [`Break`], and a cancel or the end of the connection breaks out the same way
//! at the next await. Parallel lanes, detached tails and child sessions are each a `Ctx` of their
//! own with their own share of the clock.

pub mod errors;
mod faults;
mod host;
mod output;
mod tools;
mod turn;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use futures::future::BoxFuture;
use nori_acp_kit::HostBridge;
use nori_acp_kit::Sink;
use nori_acp_kit::TurnGate;
use nori_acp_kit::acp;
use nori_acp_kit::schema::SessionId;
use nori_acp_kit::schema::SessionUpdate;
use nori_acp_kit::schema::StopReason;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::clock::Participant;
use crate::record::Record;
use crate::runtime::WorldRuntime;
use crate::session::SessionRecord;
use crate::wire::Wire;
use crate::world::AgentDef;
use crate::world::Step;
use crate::world::StopReasonDef;
use crate::world::Text;

pub(crate) use turn::run_prompt;

/// What one connection of one fake agent shares across its sessions.
pub(crate) struct Env {
    pub rt: Arc<WorldRuntime>,
    pub def: Arc<AgentDef>,
    pub wire: Wire,
    /// Cancelled when the connection ends: detached tails and sleeping tasks stop.
    pub shutdown: CancellationToken,
}

/// A session as one connection holds it.
pub(crate) struct Live {
    pub record: Arc<SessionRecord>,
    pub gate: TurnGate,
    pub host: Option<HostBridge>,
    /// A prompt of this session is running.
    pub turn_open: AtomicBool,
}

/// Why a step stopped the run.
#[derive(Debug)]
pub(crate) enum Break {
    /// The user cancelled and the agent noticed.
    Cancelled,
    /// A step ended the turn with this stop reason.
    Stop(StopReason),
    /// A step failed the turn.
    Fail(acp::Error),
    /// A step never lets the turn end.
    Hang,
    /// The connection was closed on purpose.
    Crashed,
    /// The connection ended under the agent.
    Gone,
}

pub(crate) type Run = Result<(), Break>;

/// The session a task speaks for.
#[derive(Clone)]
pub(crate) struct Lane {
    pub id: SessionId,
    pub sink: Sink,
    /// Updates are kept in the session's history for replay at `session/load`.
    pub recorded: bool,
}

/// The prompt a task is answering, for `{{prompt}}` and `{{index}}`.
pub(crate) struct TurnInfo {
    pub prompt: String,
    pub index: i64,
}

#[derive(Clone)]
pub(crate) struct Ctx {
    pub env: Arc<Env>,
    pub live: Arc<Live>,
    pub lane: Lane,
    pub participant: Arc<Participant>,
    /// Set while a cancel should interrupt this task.
    pub cancel: Option<CancellationToken>,
    pub turn: Arc<TurnInfo>,
    /// Variables of the enclosing `repeat`s.
    pub scope: Arc<BTreeMap<String, String>>,
}

impl From<StopReasonDef> for StopReason {
    fn from(reason: StopReasonDef) -> Self {
        match reason {
            StopReasonDef::EndTurn => Self::EndTurn,
            StopReasonDef::MaxTokens => Self::MaxTokens,
            StopReasonDef::MaxTurnRequests => Self::MaxTurnRequests,
            StopReasonDef::Refusal => Self::Refusal,
            StopReasonDef::Cancelled => Self::Cancelled,
        }
    }
}

impl Ctx {
    pub(crate) fn agent(&self) -> &str {
        &self.env.def.slug
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.live.record.id
    }

    pub(crate) fn record(&self, record: Record) {
        self.env
            .rt
            .recorder
            .push(self.env.rt.clock.elapsed(), record);
    }

    /// The script and the world disagreed. Recorded, never hidden.
    pub(crate) fn diverge(&self, message: impl Into<String>) {
        self.record(Record::Divergence {
            agent: self.agent().to_owned(),
            session: self.session_id().to_owned(),
            message: message.into(),
        });
    }

    /// The task is wanted no more: the connection ended, or the user cancelled it.
    pub(crate) fn checkpoint(&self) -> Run {
        if self.env.shutdown.is_cancelled() {
            return Err(Break::Gone);
        }
        if self
            .cancel
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            return Err(Break::Cancelled);
        }
        Ok(())
    }

    async fn interrupted(&self) -> Break {
        tokio::select! {
            () = self.env.shutdown.cancelled() => Break::Gone,
            () = async {
                match &self.cancel {
                    Some(token) => token.cancelled().await,
                    None => std::future::pending().await,
                }
            } => Break::Cancelled,
        }
    }

    /// Sleep for `duration` of the world's time, unless interrupted.
    pub(crate) async fn sleep(&self, duration: Duration) -> Run {
        tokio::select! {
            () = self.participant.sleep(duration) => Ok(()),
            interruption = self.interrupted() => Err(interruption),
        }
    }

    /// Wait for the outside world (a permission answer, a host tool), not interruptible by a
    /// cancel: the client decides whether the question still stands.
    pub(crate) async fn outside<F: std::future::Future>(
        &self,
        future: F,
    ) -> Result<F::Output, Break> {
        tokio::select! {
            output = self.participant.external(future) => Ok(output),
            () = self.env.shutdown.cancelled() => Err(Break::Gone),
        }
    }

    /// Wait for something that never ends (a hang) until the connection does.
    pub(crate) async fn forever(&self) {
        tokio::select! {
            () = self.participant.park(std::future::pending::<()>()) => {}
            () = self.env.shutdown.cancelled() => {}
        }
    }

    /// Send an update on this task's lane, keeping it for replay when the lane is recorded.
    pub(crate) fn emit(&self, update: SessionUpdate) -> Run {
        if self.lane.recorded
            && let Ok(json) = serde_json::to_value(&update)
        {
            self.remember(json);
        }
        self.lane.sink.update(update).map_err(|_| Break::Gone)
    }

    /// Send any `session/update` payload on `session`, through the same path as typed updates so
    /// order is kept.
    pub(crate) fn emit_raw(&self, session: &SessionId, update: Value) -> Run {
        let params = serde_json::json!({"sessionId": session, "update": update});
        if self.lane.recorded && *session == self.lane.id {
            self.remember(params["update"].clone());
        }
        let message =
            acp::UntypedMessage::new("session/update", params).map_err(|_| Break::Gone)?;
        self.lane
            .sink
            .cx()
            .send_notification(message)
            .map_err(|_| Break::Gone)
    }

    fn remember(&self, update: Value) {
        if let Some(turn) = self.live.record.state().history.last_mut() {
            turn.updates.push(update);
        }
    }

    /// `text` with `{{variables}}` substituted. Locals of a `repeat` first, then the turn's own
    /// (`prompt`, `index`, `session`, `agent`, `model`), then values the script bound. An unbound
    /// name stays as written and is recorded as a divergence.
    pub(crate) fn render(&self, text: &str) -> String {
        if !text.contains("{{") {
            return text.to_owned();
        }
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find("{{") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            let Some(end) = after.find("}}") else {
                out.push_str(&rest[start..]);
                return out;
            };
            let name = after[..end].trim();
            match self.variable(name) {
                Some(value) => out.push_str(&value),
                None => {
                    self.diverge(format!(
                        "the script uses {{{{{name}}}}}, which is not bound"
                    ));
                    out.push_str(&rest[start..start + 2 + end + 2]);
                }
            }
            rest = &after[end + 2..];
        }
        out.push_str(rest);
        out
    }

    fn variable(&self, name: &str) -> Option<String> {
        let (root, path) = name
            .split_once('.')
            .map_or((name, None), |(root, path)| (root, Some(path)));
        if path.is_none()
            && let Some(local) = self.scope.get(root)
        {
            return Some(local.clone());
        }
        match (root, path) {
            ("prompt", None) => return Some(self.turn.prompt.clone()),
            ("index", None) => return Some(self.turn.index.to_string()),
            ("session", None) => return Some(self.session_id().to_owned()),
            ("agent", None) => return Some(self.agent().to_owned()),
            ("model", None) => return self.live.record.model(),
            ("now", None) => return Some(self.env.rt.clock.now_rfc3339()),
            _ => {}
        }
        let bound = self.live.record.state().vars.get(root).cloned()?;
        let mut value = &bound;
        if let Some(path) = path {
            for key in path.split('.') {
                value = value.get(key)?;
            }
        }
        Some(match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
    }

    /// A script text, expanded and rendered.
    pub(crate) fn text(&self, text: &Text) -> String {
        self.render(&text.expand())
    }

    /// Run `steps` in order.
    pub(crate) fn run<'a>(&'a self, steps: &'a [Step]) -> BoxFuture<'a, Run> {
        Box::pin(async move {
            for step in steps {
                self.checkpoint()?;
                self.step(step).await?;
            }
            Ok(())
        })
    }

    async fn step(&self, step: &Step) -> Run {
        match step {
            Step::Say(say) => self.say(say, false).await,
            Step::Think(say) => self.say(say, true).await,
            Step::Wait(wait) => self.sleep(wait.duration.0).await,
            Step::Tool(tool) => self.tool(tool, false).await,
            Step::ToolStart(start) => self.tool_start(start),
            Step::ToolUpdate(update) => self.tool_update(update),
            Step::Permission(permission) => self.permission(permission).await,
            Step::Plan(plan) => self.plan(plan),
            Step::Usage(usage) => self.usage(usage),
            Step::SessionInfo(info) => self.session_info(info),
            Step::Status(status) => self.status(status),
            Step::Mode(mode) => self.mode(mode),
            Step::HostTool(call) => self.host_tool(call).await,
            Step::Delegate(delegate) => self.delegate(delegate).await,
            Step::Send(send) => self.send(send).await,
            Step::Error(error) => Err(Break::Fail(errors::acp_error(error, &self.env.rt.clock))),
            Step::Stop(stop) => Err(Break::Stop(stop.reason.into())),
            Step::Hang => Err(Break::Hang),
            Step::DropResponse => self.drop_response(),
            Step::RespondTwice(twice) => self.respond_twice(twice),
            Step::StaleResponse(stale) => self.stale_response(stale),
            Step::Detach(detach) => self.detach(detach),
            Step::Crash(crash) => self.crash(crash).await,
            Step::WireNoise(noise) => self.wire_noise(noise).await,
            Step::CloseStdout => self.close_stdout().await,
            Step::Throttle(throttle) => self.throttle(throttle).await,
            Step::StderrFlood(flood) => self.stderr_flood(flood).await,
            Step::RawUpdate(raw) => self.raw_update(raw),
            Step::Par(par) => self.par(par).await,
            Step::Lane(lane) => self.lane_steps(lane).await,
            Step::Subagent(subagent) => self.subagent(subagent).await,
            Step::PeerMessage(message) => self.peer_message(message),
            Step::Repeat(repeat) => self.repeat(repeat).await,
        }
    }
}
