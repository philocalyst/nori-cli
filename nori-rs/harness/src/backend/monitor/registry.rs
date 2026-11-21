//! The monitors of one actor, and the queue of events nobody has been handed yet.
//!
//! One [`Monitors`] value belongs to one actor's orchestrator. It owns three things that must
//! agree: the monitors (armed, dormant or ended), the queue of events they produced, and at most
//! one lease: the batch being offered to the agent. Everything that decides what an agent is told
//! happens under one lock, so an event is queued once, leased once, and delivered once.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::time::Duration;
use std::time::Instant;

use serde::Serialize;
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedSender;

use super::event::Ending;
use super::event::Event;
use super::event::EventLine;
use super::journal::Record;
use super::pipeline;
use super::source::SourceEnv;
use super::spec::Action;
use super::spec::MAX_RUNNING;
use super::spec::Origin;
use super::spec::Spec;
use super::spec::WakeMode;
use super::view::Stopped;
use super::view::unknown;
use crate::session_event_fanout::SessionEventFanout;
use crate::session_event_fanout::WeakSessionEventFanout;
use crate::transcript::TranscriptRecorder;

/// Ended monitors kept in the list.
const KEEP_ENDED: usize = 16;
/// Events waiting for the agent beyond this are dropped, oldest information first.
const MAX_QUEUED: usize = 200;

/// The cell holding the session's conversation journal.
pub(crate) type Journal = Arc<tokio::sync::RwLock<Option<Arc<TranscriptRecorder>>>>;

#[derive(Clone)]
pub(crate) struct Monitors {
    pub(super) shared: Arc<Shared>,
}

impl std::fmt::Debug for Monitors {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Monitors").finish_non_exhaustive()
    }
}

pub(super) struct Shared {
    pub(super) state: Mutex<State>,
    /// Something changed that the delivery loop may need to act on.
    pub(super) changed: Notify,
    pub(super) journal: Mutex<Option<Journal>>,
    /// The session's public events, for `session` monitors.
    /// Held weakly: the monitors live as long as the backend, which must not keep the event
    /// stream open after the session has ended.
    pub(super) events: Mutex<Option<WeakSessionEventFanout>>,
    /// Where actions other than a wake go: the backend performs them.
    pub(super) effects: Mutex<Option<UnboundedSender<Effect>>>,
}

/// An action a monitor's event asks the backend to perform, with what it fired on.
#[derive(Debug, Clone)]
pub(crate) struct Effect {
    pub(crate) monitor: String,
    pub(crate) label: String,
    pub(crate) action: Action,
    /// The event's lines, for `{{event}}`.
    pub(crate) event: String,
    pub(crate) failure: bool,
}

impl Effect {
    /// `template` with `{{event}}`, `{{label}}` and `{{monitor}}` filled in.
    pub(crate) fn render(&self, template: &str) -> String {
        let mut rendered = String::with_capacity(template.len());
        let mut rest = template;
        while let Some(open) = rest.find("{{") {
            rendered.push_str(&rest[..open]);
            rest = &rest[open..];
            let Some(close) = rest.find("}}") else {
                rendered.push_str(rest);
                return rendered;
            };
            let token = &rest[2..close];
            let replacement = match token {
                "event" => Some(self.event.as_str()),
                "label" => Some(self.label.as_str()),
                "monitor" => Some(self.monitor.as_str()),
                _ => None,
            };
            if let Some(replacement) = replacement {
                rendered.push_str(replacement);
            } else {
                rendered.push_str(&rest[..close + 2]);
            }
            rest = &rest[close + 2..];
        }
        rendered.push_str(rest);
        rendered
    }
}

#[derive(Default)]
pub(super) struct State {
    pub(super) next_monitor: i64,
    pub(super) next_seq: i64,
    pub(super) entries: Vec<Entry>,
    pub(super) queue: Vec<Queued>,
    /// A batch is being offered to the agent.
    pub(super) leased: bool,
    /// Events dropped from a full queue since the last wake.
    pub(super) dropped: i64,
    pub(super) wakes: VecDeque<Instant>,
    /// The latest wake turn has not ended yet.
    pub(super) monitor_turn: bool,
    pub(super) closed: bool,
}

pub(super) struct Entry {
    pub(super) id: String,
    pub(super) spec: Arc<Spec>,
    /// Increments each time the monitor starts, so a task of an earlier run cannot speak for it.
    pub(super) run: i64,
    pub(super) phase: Phase,
    pub(super) started: Instant,
    pub(super) emitted: i64,
    pub(super) failures: i64,
    pub(super) last: Option<String>,
    pub(super) task: Option<tokio::task::AbortHandle>,
}

pub(super) enum Phase {
    Running {
        deadline: Instant,
    },
    /// Armed in an earlier run; nothing watches until it is re-armed.
    Dormant,
    Ended(Ending),
}

pub(super) struct Queued {
    pub(super) event: Event,
    pub(super) at: Instant,
    /// When the event is ready to wake an idle agent; `None` waits for a turn boundary.
    pub(super) ready: Option<Instant>,
}

/// What `monitor_start` and `monitor_rearm` report.
#[derive(Debug, Serialize)]
pub(crate) struct Armed {
    pub(crate) monitor: String,
    pub(crate) label: String,
    pub(crate) source: &'static str,
    pub(crate) timeout_seconds: i64,
    pub(crate) max_events: i64,
    pub(crate) wake: &'static str,
}

/// What a re-arm changes.
#[derive(Debug, Default)]
pub(crate) struct Overrides {
    pub(crate) timeout_seconds: Option<i64>,
    pub(crate) max_events: Option<i64>,
    pub(crate) wake: Option<WakeMode>,
}

/// One monitor's burst of lines, ready to be an event.
pub(super) struct Burst {
    pub(super) lines: Vec<EventLine>,
    pub(super) omitted: i64,
}

impl State {
    pub(super) fn entry(&mut self, id: &str) -> Option<&mut Entry> {
        self.entries.iter_mut().find(|entry| entry.id == id)
    }

    pub(super) fn running(&self) -> impl Iterator<Item = &Entry> {
        self.entries
            .iter()
            .filter(|entry| matches!(entry.phase, Phase::Running { .. }))
    }

    /// End an entry that is not already over, without telling the agent.
    pub(super) fn end(&mut self, id: &str, ending: Ending) {
        if let Some(entry) = self.entry(id)
            && !matches!(entry.phase, Phase::Ended(_))
        {
            if let Some(task) = entry.task.take() {
                task.abort();
            }
            entry.phase = Phase::Ended(ending);
        }
    }

    /// Keep the list of ended monitors short: the oldest go first.
    pub(super) fn trim_ended(&mut self) {
        let ended = self
            .entries
            .iter()
            .filter(|entry| matches!(entry.phase, Phase::Ended(_)))
            .count();
        let mut surplus = ended.saturating_sub(KEEP_ENDED);
        self.entries.retain(|entry| {
            if matches!(entry.phase, Phase::Ended(_)) && surplus > 0 {
                surplus -= 1;
                false
            } else {
                true
            }
        });
    }

    pub(super) fn insert(&mut self, queued: Queued) {
        let at = self
            .queue
            .partition_point(|queued_event| queued_event.event.seq < queued.event.seq);
        self.queue.insert(at, queued);
        // A full queue loses its oldest events that would not have woken anyone.
        while self.queue.len() > MAX_QUEUED {
            let victim = self
                .queue
                .iter()
                .position(|queued| !queued.event.failure && queued.event.ending.is_none())
                .unwrap_or(0);
            self.queue.remove(victim);
            self.dropped += 1;
        }
    }

    pub(super) fn status_of(entry: &Entry) -> String {
        match &entry.phase {
            Phase::Running { .. } => "running".to_owned(),
            Phase::Dormant => "dormant".to_owned(),
            Phase::Ended(ending) => ending.status(),
        }
    }
}

impl Monitors {
    pub(crate) fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State::default()),
                changed: Notify::new(),
                journal: Mutex::new(None),
                events: Mutex::new(None),
                effects: Mutex::new(None),
            }),
        }
    }

    pub(super) fn lock(&self) -> MutexGuard<'_, State> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Journal into the session's conversation from now on.
    pub(crate) fn attach_journal(&self, journal: Journal) {
        *self
            .shared
            .journal
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(journal);
    }

    /// Let `session` monitors read the session's events from now on.
    pub(crate) fn attach_events(&self, events: SessionEventFanout) {
        *self
            .shared
            .events
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(events.downgrade());
    }

    /// A subscription to the session's events, when the session publishes them.
    pub(crate) fn subscribe_events(
        &self,
    ) -> Option<tokio::sync::mpsc::Receiver<nori_protocol::SessionEvent>> {
        self.shared
            .events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .and_then(WeakSessionEventFanout::upgrade)
            .as_ref()
            .map(SessionEventFanout::subscribe)
    }

    /// Hand actions other than a wake to the backend from now on.
    pub(crate) fn attach_effects(&self, effects: UnboundedSender<Effect>) {
        *self
            .shared
            .effects
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(effects);
    }

    /// Ask the backend to perform `effect`. Without a backend (a test) it is logged and dropped.
    pub(super) fn perform(&self, effect: Effect) {
        let effects = self
            .shared
            .effects
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        match effects {
            Some(effects) => {
                let _ = effects.send(effect);
            }
            None => {
                tracing::warn!(monitor = %effect.monitor, "a monitor action has no backend to run it")
            }
        }
    }

    pub(super) async fn write(&self, record: &Record) {
        let journal = self
            .shared
            .journal
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let Some(journal) = journal else { return };
        let Some(recorder) = journal.read().await.clone() else {
            return;
        };
        match serde_json::to_string(record) {
            Ok(payload) => {
                if let Err(error) = recorder
                    .record_command(super::journal::COMMAND, payload)
                    .await
                {
                    tracing::warn!(%error, "failed to journal a monitor change");
                }
            }
            Err(error) => tracing::warn!(%error, "failed to serialize a monitor change"),
        }
    }

    /// Wait until something changes that the delivery loop may need to act on.
    pub(crate) async fn changed(&self) {
        self.shared.changed.notified().await;
    }

    pub(crate) fn notify(&self) {
        self.shared.changed.notify_one();
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.lock().closed
    }
    // ---- arming --------------------------------------------------------------------------

    /// Arm a monitor and start its source. With `replace`, a running monitor of the same label is
    /// ended quietly and this one takes its place.
    pub(crate) async fn start(
        &self,
        spec: Spec,
        env: SourceEnv,
        replace: bool,
    ) -> Result<Armed, String> {
        spec.validate_actions()?;
        let rules = spec.rules.compile()?;
        let spec = Arc::new(spec);
        let mut superseded = None;
        let (id, run, armed) = {
            let mut state = self.lock();
            if state.closed {
                return Err("this session is ending; no monitor can be armed".to_owned());
            }
            let clash = state
                .running()
                .find(|entry| entry.spec.label == spec.label)
                .map(|entry| entry.id.clone());
            match clash {
                Some(id) if replace => {
                    state.end(&id, Ending::Superseded);
                    superseded = Some(id);
                }
                Some(id) => {
                    return Err(format!(
                        "a monitor labelled `{}` is already running ({id}). `monitor_stop` it, \
                         `monitor_rearm` it to extend it, or use another label.",
                        spec.label
                    ));
                }
                None => {}
            }
            // Standing monitors (a module's) do not take the agent's places.
            let running: Vec<String> = state
                .running()
                .filter(|entry| entry.spec.origin == Origin::Agent)
                .map(|entry| format!("{} ({})", entry.id, entry.spec.label))
                .collect();
            if spec.origin == Origin::Agent
                && i64::try_from(running.len()).unwrap_or(i64::MAX) >= MAX_RUNNING
            {
                return Err(format!(
                    "{MAX_RUNNING} monitors are already running: {}. `monitor_stop` one first.",
                    running.join(", ")
                ));
            }
            state.next_monitor += 1;
            let id = format!("mon-{}", state.next_monitor);
            let armed = Armed {
                monitor: id.clone(),
                label: spec.label.clone(),
                source: spec.source.kind(),
                timeout_seconds: spec.limits.timeout_seconds,
                max_events: spec.limits.max_events,
                wake: spec.wake.label(),
            };
            state.entries.push(Entry {
                id: id.clone(),
                spec: Arc::clone(&spec),
                run: 1,
                phase: Phase::Running {
                    deadline: Instant::now() + seconds(spec.limits.timeout_seconds),
                },
                started: Instant::now(),
                emitted: 0,
                failures: 0,
                last: None,
                task: None,
            });
            state.trim_ended();
            (id, 1, armed)
        };
        if let Some(id) = superseded {
            self.write(&Record::Ended {
                id,
                ending: Ending::Superseded,
            })
            .await;
        }
        self.write(&Record::Armed {
            id: id.clone(),
            spec: (*spec).clone(),
        })
        .await;
        self.spawn(&id, run, spec, rules, env);
        self.notify();
        Ok(armed)
    }

    fn spawn(
        &self,
        id: &str,
        run: i64,
        spec: Arc<Spec>,
        rules: super::spec::CompiledRules,
        env: SourceEnv,
    ) {
        let task = tokio::spawn(pipeline::run(
            self.clone(),
            id.to_owned(),
            run,
            spec,
            rules,
            env,
        ))
        .abort_handle();
        if let Some(entry) = self.lock().entry(id) {
            entry.task = Some(task);
        }
    }

    /// The spec of the monitor `which` names (an id or a label), and whether it is running.
    pub(crate) fn find(&self, which: &str) -> Result<(String, Spec, bool), String> {
        let state = self.lock();
        let hit = state
            .entries
            .iter()
            .rev()
            .find(|entry| entry.id == which || entry.spec.label == which);
        match hit {
            Some(entry) => Ok((
                entry.id.clone(),
                (*entry.spec).clone(),
                matches!(entry.phase, Phase::Running { .. }),
            )),
            None => Err(unknown(&state, which)),
        }
    }

    /// Restart an ended or dormant monitor with a fresh timeout and event budget, or extend a
    /// running one's timeout. A restart needs `env`, the prepared source.
    pub(crate) async fn rearm(
        &self,
        id: &str,
        overrides: Overrides,
        env: Option<SourceEnv>,
    ) -> Result<Armed, String> {
        let (armed, restarted) = {
            let mut state = self.lock();
            if state.closed {
                return Err("this session is ending; no monitor can be armed".to_owned());
            }
            let running = i64::try_from(state.running().count()).unwrap_or(i64::MAX);
            let Some(entry) = state.entry(id) else {
                return Err(format!("`{id}` is not a monitor of yours"));
            };
            let mut spec = (*entry.spec).clone();
            if let Some(seconds) = overrides.timeout_seconds {
                spec.limits.timeout_seconds = seconds;
            }
            if let Some(max_events) = overrides.max_events {
                spec.limits.max_events = max_events;
            }
            if let Some(wake) = overrides.wake {
                spec.wake = wake;
            }
            spec.validate_actions()?;
            let armed = Armed {
                monitor: entry.id.clone(),
                label: spec.label.clone(),
                source: spec.source.kind(),
                timeout_seconds: spec.limits.timeout_seconds,
                max_events: spec.limits.max_events,
                wake: spec.wake.label(),
            };
            let deadline = Instant::now() + seconds(spec.limits.timeout_seconds);
            match &mut entry.phase {
                Phase::Running { deadline: current } => {
                    *current = deadline;
                    (armed, None)
                }
                Phase::Dormant | Phase::Ended(_) => {
                    if running >= MAX_RUNNING {
                        return Err(format!(
                            "{MAX_RUNNING} monitors are already running. `monitor_stop` one first."
                        ));
                    }
                    if env.is_none() {
                        return Err("this monitor's source could not be prepared".to_owned());
                    }
                    entry.run += 1;
                    entry.phase = Phase::Running { deadline };
                    entry.started = Instant::now();
                    entry.emitted = 0;
                    entry.failures = 0;
                    entry.last = None;
                    entry.spec = Arc::new(spec);
                    (armed, Some((entry.run, Arc::clone(&entry.spec))))
                }
            }
        };
        if let (Some((run, spec)), Some(env)) = (restarted, env) {
            let rules = spec.rules.compile()?;
            self.write(&Record::Armed {
                id: id.to_owned(),
                spec: (*spec).clone(),
            })
            .await;
            self.spawn(id, run, spec, rules, env);
            self.notify();
        }
        Ok(armed)
    }
    // ---- stopping --------------------------------------------------------------------------

    /// End the monitor `which` names, or all of them. Events it already queued stay queued.
    pub(crate) async fn stop(&self, which: &str, ending: Ending) -> Result<Vec<Stopped>, String> {
        let stopped = {
            let mut state = self.lock();
            let ids: Vec<String> = if which == "all" {
                // Everything the agent armed; a module's monitors are stopped by name only.
                state
                    .entries
                    .iter()
                    .filter(|entry| {
                        !matches!(entry.phase, Phase::Ended(_))
                            && entry.spec.origin == Origin::Agent
                    })
                    .map(|entry| entry.id.clone())
                    .collect()
            } else {
                match state.entries.iter().rev().find(|entry| {
                    stop_matches(&entry.id, &entry.spec.label, &entry.spec.origin, which)
                }) {
                    Some(entry) => vec![entry.id.clone()],
                    None => return Err(unknown(&state, which)),
                }
            };
            let mut stopped = Vec::new();
            for id in ids {
                if let Some(entry) = state.entry(&id) {
                    if let Phase::Ended(done) = &entry.phase {
                        return Err(format!(
                            "{} (`{}`) already ended: {}. `monitor_rearm` starts it again.",
                            entry.id,
                            entry.spec.label,
                            done.status()
                        ));
                    }
                    stopped.push(Stopped {
                        monitor: entry.id.clone(),
                        label: entry.spec.label.clone(),
                        events: entry.emitted,
                        failures: entry.failures,
                    });
                }
                state.end(&id, ending.clone());
            }
            stopped
        };
        for monitor in &stopped {
            self.write(&Record::Ended {
                id: monitor.monitor.clone(),
                ending: ending.clone(),
            })
            .await;
        }
        self.notify();
        Ok(stopped)
    }

    /// End everything without a word and without journaling an end: the session is over, and the
    /// journal keeps these monitors armed, so a resume finds them dormant.
    pub(crate) fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
        for entry in &mut state.entries {
            if let Some(task) = entry.task.take() {
                task.abort();
            }
        }
        drop(state);
        self.notify();
    }
}

/// The agent can address its own monitors by id or label. A module's standing monitors have a
/// stable label; their generated ids are internal to that module's session.
fn stop_matches(id: &str, label: &str, origin: &Origin, which: &str) -> bool {
    label == which || (origin == &Origin::Agent && id == which)
}

pub(super) fn seconds(seconds: i64) -> Duration {
    Duration::from_secs(u64::try_from(seconds.max(0)).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn a_standing_monitor_can_be_stopped_by_label_but_not_its_internal_id() {
        let module = Origin::Module {
            path: PathBuf::from("monitor.pkl"),
        };
        assert!(stop_matches("mon-1", "daily", &module, "daily"));
        assert!(!stop_matches("mon-1", "daily", &module, "mon-1"));
        assert!(stop_matches("mon-2", "tests", &Origin::Agent, "mon-2"));
        assert!(stop_matches("mon-2", "tests", &Origin::Agent, "tests"));
    }

    #[test]
    fn event_text_is_not_interpreted_as_template_syntax() {
        let effect = Effect {
            monitor: "mon-1".to_owned(),
            label: "tests".to_owned(),
            action: Action::Notify {
                title: None,
                body: None,
            },
            event: "literal {{label}} {{monitor}}".to_owned(),
            failure: false,
        };
        assert_eq!(
            effect.render("{{event}} for {{label}}"),
            "literal {{label}} {{monitor}} for tests"
        );
    }
}
