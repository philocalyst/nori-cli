//! The driver: one real harness session against a world of fake agents.
//!
//! [`WorldSession::boot`] starts the real `AcpBackend` (orchestrator, approvals, transcripts in a
//! temporary `NORI_HOME`) on the world's default agent. The test acts as the user (prompt,
//! approve, deny, cancel, steer, rewind, resume) and reads what happened as [`Events`], as the
//! agents' [`Entry`] flight recorder, and as [`Violation`]s of the invariants.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use nori_config::NoriConfig;
use nori_fake_agent::ClockMode;
use nori_fake_agent::Entry;
use nori_fake_agent::InstalledWorld;
use nori_fake_agent::Record;
use nori_fake_agent::World;
use nori_harness::runtime::AgentPrepareSpec;
use nori_harness::runtime::HarnessHandle;
use nori_harness::runtime::LaunchedSession;
use nori_harness::runtime::SessionResume;
use nori_harness::runtime::SessionStart;
use nori_harness::runtime::prepare_and_launch_session;
use nori_protocol::AcpEvent;
use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;
use nori_protocol::SessionPhase;
use nori_protocol::SubagentState;
use nori_protocol::acp::v1 as acp;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::watch;

use crate::approvals;
use crate::approvals::Approvals;
use crate::approvals::Decision;
use crate::approvals::PendingPermission;
use crate::boot::Claim;
use crate::boot::ProcessAgent;
use crate::boot::Transport;
use crate::boot::WorldOptions;
use crate::events::Events;
use crate::events::Recorded;
use crate::invariants;
use crate::invariants::Check;
use crate::invariants::Violation;

/// Why a wait ended without what it waited for.
#[derive(Debug, thiserror::Error)]
pub enum Stuck {
    #[error("the session did not settle within {0:?}: {1}")]
    Timeout(Duration, String),
    #[error(
        "the session waits on {} permission request(s) and nobody answers them: {}",
        .0.len(),
        .0.iter().map(|p| p.tool.as_str()).collect::<Vec<_>>().join(", ")
    )]
    AwaitingUser(Vec<PendingPermission>),
    #[error("the session ended before the wait was over")]
    Ended,
}

struct Shared {
    booted: Instant,
    events: Mutex<Vec<Arc<Recorded>>>,
    changed: watch::Sender<i64>,
    pending: Mutex<Vec<PendingPermission>>,
    approvals: Approvals,
    last_activity: Mutex<Instant>,
    transcript_id: Mutex<Option<String>>,
    acp_session: Mutex<Option<acp::SessionId>>,
    ended: AtomicBool,
    recorder: nori_fake_agent::FlightRecorder,
}

impl Shared {
    fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn record(&self, event: &SessionEvent) -> i64 {
        let seq = {
            let mut events = Self::lock(&self.events);
            let seq = i64::try_from(events.len()).unwrap_or(i64::MAX);
            events.push(Arc::new(Recorded {
                seq,
                at: self.booted.elapsed(),
                event: event.clone(),
            }));
            seq
        };
        *Self::lock(&self.last_activity) = Instant::now();
        self.changed.send_replace(seq);
        seq
    }
}

/// A running session against a world.
pub struct WorldSession {
    shared: Arc<Shared>,
    handle: Mutex<HarnessHandle>,
    pump: Mutex<Option<tokio::task::JoinHandle<()>>>,
    config: Arc<NoriConfig>,
    world: InstalledWorld,
    timing: Timing,
    // Held so the temporary home outlives the session, and the claim is released last.
    _home: tempfile::TempDir,
    _claim: Claim,
}

struct Timing {
    quiet: Duration,
    timeout: Duration,
}

fn text(content: &str) -> Vec<acp::ContentBlock> {
    vec![acp::ContentBlock::Text(acp::TextContent::new(content))]
}

fn prepare_spec(config: &Arc<NoriConfig>) -> AgentPrepareSpec {
    AgentPrepareSpec {
        handoff: None,
        config: Arc::clone(config),
        cli_version: "nori-world".to_owned(),
        session_context: None,
        initial_context: None,
    }
}

/// What a wait looks at each time something happens.
struct Snapshot {
    events: Events,
    quiet_for: Duration,
    pending: Vec<PendingPermission>,
}

impl WorldSession {
    /// Boot `world` and start a session on its default agent (or `options.agent`).
    pub async fn boot(world: World, options: WorldOptions) -> anyhow::Result<Self> {
        let WorldOptions {
            transport,
            approvals,
            agent,
            clock,
            quiet,
            timeout,
            configure,
        } = options;
        let agent = agent
            .or_else(|| world.default_agent().map(|agent| agent.slug.clone()))
            .context("the world defines no agents")?;
        let installed = InstalledWorld::new(world, clock);
        let home = tempfile::tempdir().context("temporary NORI_HOME")?;
        // A world run as separate processes is a file they read and a recorder they append to.
        let process = match &transport {
            Transport::InProcess => None,
            Transport::Process { nori } => {
                let world_file = home.path().join("world.json");
                std::fs::write(&world_file, serde_json::to_string(installed.world())?)
                    .context("the world file for the served agents")?;
                Some((
                    ProcessAgent {
                        nori: nori.clone(),
                        record_file: home.path().join("recorder.jsonl"),
                    },
                    world_file,
                ))
            }
        };
        let entries = match &process {
            None => installed.entries(),
            Some((_, world_file)) => installed.world().agent_entries(
                &world_file.display().to_string(),
                Some(&match clock {
                    ClockMode::Virtual => "virtual".to_owned(),
                    ClockMode::Real(speed) => format!("x{speed}"),
                }),
            ),
        };
        let claim = Claim::take(
            &entries,
            process.as_ref().map(|(process, _)| process),
            timeout,
        )
        .await?;

        let cwd: PathBuf = home.path().join("work");
        std::fs::create_dir_all(&cwd).context("work directory")?;
        let nori_home = home.path().join("home");
        std::fs::create_dir_all(&nori_home).context("nori home")?;
        if let Some(policy) = &installed.world().policy {
            std::fs::write(nori_home.join("init.pkl"), policy).context("the world's policy")?;
        }
        let mut config = NoriConfig {
            active_agent: agent.clone(),
            agent,
            cwd,
            nori_home,
            approval_policy: match approvals {
                Approvals::AllowAll => nori_config::AskForApproval::Never,
                Approvals::Manual | Approvals::DenyAll | Approvals::Script(_) => {
                    nori_config::AskForApproval::OnRequest
                }
            },
            ..NoriConfig::default()
        };
        if let Some(configure) = configure {
            configure(&mut config);
        }
        let config = Arc::new(config);

        let (changed, _) = watch::channel(-1);
        let shared = Arc::new(Shared {
            booted: Instant::now(),
            events: Mutex::default(),
            changed,
            pending: Mutex::default(),
            approvals,
            last_activity: Mutex::new(Instant::now()),
            transcript_id: Mutex::default(),
            acp_session: Mutex::default(),
            ended: AtomicBool::new(false),
            recorder: match &process {
                None => installed.runtime().recorder.clone(),
                Some((process, _)) => {
                    nori_fake_agent::FlightRecorder::file_backed(process.record_file.clone())
                }
            },
        });
        let launched = prepare_and_launch_session(prepare_spec(&config), SessionStart::New);
        let session = Self {
            shared,
            handle: Mutex::new(launched.handle.clone()),
            pump: Mutex::new(None),
            config,
            world: installed,
            timing: Timing { quiet, timeout },
            _home: home,
            _claim: claim,
        };
        session.attach(launched);
        session
            .wait_for("the session to start", |events| {
                events.root_session().is_some()
            })
            .await?;
        Ok(session)
    }

    /// Start pumping a launched session's events into the record.
    fn attach(&self, launched: LaunchedSession) {
        let LaunchedSession { handle, events } = launched;
        *Shared::lock(&self.handle) = handle.clone();
        self.shared.ended.store(false, Ordering::SeqCst);
        let pump = tokio::spawn(pump(events, handle, Arc::clone(&self.shared)));
        *Shared::lock(&self.pump) = Some(pump);
    }

    /// The harness handle, for anything the driver has no verb for.
    pub fn handle(&self) -> HarnessHandle {
        Shared::lock(&self.handle).clone()
    }

    pub fn world(&self) -> &InstalledWorld {
        &self.world
    }

    /// The world's clock, which the fake agents' sleeps run on.
    pub fn clock(&self) -> &nori_fake_agent::Clock {
        &self.world.runtime().clock
    }

    pub fn nori_home(&self) -> &std::path::Path {
        &self.config.nori_home
    }

    pub fn cwd(&self) -> &std::path::Path {
        &self.config.cwd
    }

    // ---- what happened

    /// Every event so far.
    pub fn events(&self) -> Events {
        Events::new(Shared::lock(&self.shared.events).clone())
    }

    /// What the agents saw and did, with what the user did in the same order.
    pub fn recorder(&self) -> Vec<Entry> {
        self.shared.recorder.snapshot()
    }

    /// Where the script and the world disagreed (see `Record::Divergence`).
    pub fn divergences(&self) -> Vec<String> {
        self.shared.recorder.divergences()
    }

    pub fn pending_permissions(&self) -> Vec<PendingPermission> {
        Shared::lock(&self.shared.pending).clone()
    }

    /// Every invariant that does not hold right now. Properties that only hold once the session
    /// has settled are checked only if it has.
    pub fn violations(&self) -> Vec<Violation> {
        let snapshot = self.snapshot();
        let settled = Self::is_quiet(&snapshot.events) && snapshot.pending.is_empty();
        invariants::check(&snapshot.events, &self.recorder(), Check { settled })
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            events: self.events(),
            quiet_for: Shared::lock(&self.shared.last_activity).elapsed(),
            pending: self.pending_permissions(),
        }
    }

    /// The turn is over and no child is running.
    fn is_quiet(events: &Events) -> bool {
        let idle = matches!(events.phase(), None | Some(SessionPhase::Idle));
        let children_idle = events
            .subagents()
            .iter()
            .all(|info| !matches!(info.state, Some(SubagentState::Running)));
        idle && children_idle
    }

    // ---- waiting

    async fn until(
        &self,
        what: &str,
        mut ready: impl FnMut(&Snapshot) -> bool,
    ) -> Result<Events, Stuck> {
        let started = Instant::now();
        let mut changes = self.shared.changed.subscribe();
        loop {
            let snapshot = self.snapshot();
            if ready(&snapshot) {
                return Ok(snapshot.events);
            }
            if self.shared.ended.load(Ordering::SeqCst) {
                return Err(Stuck::Ended);
            }
            let awaiting_user = !snapshot.pending.is_empty()
                && snapshot.quiet_for >= self.timing.quiet
                && !matches!(self.shared.approvals, Approvals::AllowAll);
            if awaiting_user {
                return Err(Stuck::AwaitingUser(snapshot.pending));
            }
            if started.elapsed() > self.timing.timeout {
                return Err(Stuck::Timeout(
                    self.timing.timeout,
                    format!(
                        "waiting for {what}; phase {:?}, {} events, quiet for {:?}",
                        snapshot.events.phase(),
                        snapshot.events.len(),
                        snapshot.quiet_for
                    ),
                ));
            }
            let nap = self.timing.quiet.min(Duration::from_millis(50));
            tokio::select! {
                _ = changes.changed() => {}
                () = tokio::time::sleep(nap) => {}
            }
        }
    }

    /// Wait until `ready` holds of the events.
    pub async fn wait_for(
        &self,
        what: &str,
        mut ready: impl FnMut(&Events) -> bool,
    ) -> Result<Events, Stuck> {
        self.until(what, |snapshot| ready(&snapshot.events)).await
    }

    /// Wait until nothing is running and nothing has happened for a quiet moment. Fails with
    /// [`Stuck::AwaitingUser`] when the session waits on a permission request no one answers.
    pub async fn settle(&self) -> Result<Events, Stuck> {
        let quiet = self.timing.quiet;
        self.until("the session to settle", |snapshot| {
            snapshot.quiet_for >= quiet
                && snapshot.pending.is_empty()
                && Self::is_quiet(&snapshot.events)
        })
        .await
    }

    // ---- acting as the user

    fn note(&self, action: &str, detail: impl Into<String>) {
        self.shared.recorder.push(
            self.clock().elapsed(),
            Record::User {
                action: action.to_owned(),
                detail: detail.into(),
            },
        );
    }

    /// Submit a prompt and return its request id without waiting.
    pub async fn submit(&self, prompt: &str) -> anyhow::Result<acp::RequestId> {
        self.note("prompt", prompt);
        self.handle().prompt(text(prompt)).await
    }

    /// Submit a prompt, wait for the turn's response, then settle. Returns everything that
    /// happened since the prompt.
    pub async fn ask(&self, prompt: &str) -> anyhow::Result<Events> {
        let before = i64::try_from(self.events().len()).unwrap_or(i64::MAX) - 1;
        let request = self.submit(prompt).await?;
        self.wait_for("the prompt's response", |events| {
            events.responses().iter().any(|(id, _)| *id == request)
        })
        .await?;
        let events = self.settle().await?;
        Ok(events.since(before))
    }

    /// Interrupt the running turn and start this prompt ahead of the queue.
    pub async fn steer(&self, prompt: &str) -> anyhow::Result<acp::RequestId> {
        self.note("steer", prompt);
        self.handle().steer_content(text(prompt)).await
    }

    pub async fn cancel(&self) -> anyhow::Result<()> {
        self.note("cancel", "");
        self.handle().cancel().await
    }

    /// Kill the agent's connection, as the user's "stop the agent" does.
    pub async fn stop_agent(&self) -> anyhow::Result<()> {
        self.note("stop_agent", "");
        self.handle().stop_agent().await
    }

    /// Answer the oldest pending permission request with `decision`.
    pub async fn answer(&self, decision: Decision) -> anyhow::Result<PendingPermission> {
        let request = {
            let mut pending = Shared::lock(&self.shared.pending);
            anyhow::ensure!(
                !pending.is_empty(),
                "no permission request is waiting for an answer"
            );
            pending.remove(0)
        };
        respond(&self.handle(), &self.shared, &request, decision, "test").await?;
        Ok(request)
    }

    pub async fn approve(&self) -> anyhow::Result<PendingPermission> {
        self.answer(Decision::Allow).await
    }

    pub async fn deny(&self) -> anyhow::Result<PendingPermission> {
        self.answer(Decision::Deny).await
    }

    /// Wait for a permission request to arrive, then return it (it stays pending).
    pub async fn next_permission(&self) -> Result<PendingPermission, Stuck> {
        self.until("a permission request", |snapshot| {
            !snapshot.pending.is_empty()
        })
        .await
        .map(|_| self.pending_permissions().remove(0))
    }

    /// Rewind the conversation to record `head` (the end when absent) and bind a fresh agent
    /// session to it.
    pub async fn rewind(&self, head: Option<i64>) -> anyhow::Result<()> {
        let transcript = Shared::lock(&self.shared.transcript_id)
            .clone()
            .context("the session has no transcript yet")?;
        self.note("rewind", format!("{head:?}"));
        self.handle().rewind_and_rebind(transcript, head).await
    }

    /// End this harness session and resume the same agent session in a new one, as quitting and
    /// running `nori resume` does. Events keep accumulating in the same record.
    pub async fn resume(&self) -> anyhow::Result<()> {
        let acp_session_id = Shared::lock(&self.shared.acp_session)
            .as_ref()
            .map(ToString::to_string)
            .context("the session has no agent session to resume")?;
        self.note("resume", acp_session_id.clone());
        let transcript_id = Shared::lock(&self.shared.transcript_id).clone();
        self.end_current().await?;
        // `nori resume` hands the session its journaled transcript; so does this.
        let transcript = match transcript_id {
            Some(id) => {
                let loader = nori_harness::TranscriptLoader::new(self.config.nori_home.clone());
                match loader.find_session_metadata_by_id(&id).await? {
                    Some(meta) => Some(
                        loader
                            .load_transcript(&meta.project_id, &meta.session_id)
                            .await?,
                    ),
                    None => None,
                }
            }
            None => None,
        };
        let launched = prepare_and_launch_session(
            prepare_spec(&self.config),
            SessionStart::Resume(SessionResume {
                acp_session_id: Some(acp_session_id),
                transcript,
            }),
        );
        self.attach(launched);
        Ok(())
    }

    async fn end_current(&self) -> anyhow::Result<()> {
        self.handle().shutdown().await?;
        let pump = Shared::lock(&self.pump).take();
        if let Some(pump) = pump {
            let _ = tokio::time::timeout(self.timing.timeout, pump).await;
        }
        Ok(())
    }

    /// Shut the session down and return everything that happened.
    pub async fn finish(self) -> anyhow::Result<Report> {
        self.note("shutdown", "");
        let settled = {
            let snapshot = self.snapshot();
            Self::is_quiet(&snapshot.events) && snapshot.pending.is_empty()
        };
        let events = self.events();
        let recorder = self.recorder();
        let violations = invariants::check(&events, &recorder, Check { settled });
        let divergences = self.divergences();
        self.end_current().await?;
        Ok(Report {
            events: self.events(),
            recorder: self.recorder(),
            violations,
            divergences,
        })
    }
}

impl Drop for WorldSession {
    fn drop(&mut self) {
        let handle = Shared::lock(&self.handle).clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = handle.shutdown().await;
            });
        }
        if let Some(pump) = Shared::lock(&self.pump).take() {
            pump.abort();
        }
    }
}

/// What a finished session leaves behind.
#[derive(Debug)]
pub struct Report {
    pub events: Events,
    pub recorder: Vec<Entry>,
    pub violations: Vec<Violation>,
    pub divergences: Vec<String>,
}

impl Report {
    /// Panic-free assertion helper: `Ok` when no invariant was violated and the script and the
    /// world never disagreed.
    pub fn is_clean(&self) -> Result<(), String> {
        let mut problems: Vec<String> = self.violations.iter().map(ToString::to_string).collect();
        problems.extend(self.divergences.iter().map(|d| format!("divergence: {d}")));
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("\n"))
        }
    }
}

async fn respond(
    handle: &HarnessHandle,
    shared: &Shared,
    request: &PendingPermission,
    decision: Decision,
    who: &str,
) -> anyhow::Result<()> {
    let outcome = approvals::outcome(request, decision).with_context(|| {
        format!(
            "the request for {:?} offers no option that {decision:?} can pick: {:?}",
            request.tool, request.options
        )
    })?;
    shared.recorder.push(
        std::time::Duration::ZERO,
        Record::User {
            action: match decision {
                Decision::Allow => "approve",
                Decision::Deny => "deny",
                Decision::Cancel => "withdraw",
            }
            .to_owned(),
            detail: format!("{who}: {}", request.tool),
        },
    );
    handle
        .respond_to_agent(
            request.request_id.clone(),
            Ok(acp::ClientResponse::RequestPermissionResponse(
                acp::RequestPermissionResponse::new(outcome),
            )),
        )
        .await
}

async fn pump(
    mut events: UnboundedReceiver<SessionEvent>,
    handle: HarnessHandle,
    shared: Arc<Shared>,
) {
    while let Some(event) = events.recv().await {
        let seq = shared.record(&event);
        match &event {
            SessionEvent::Nori(NoriEvent::SessionStarted(started)) => {
                *Shared::lock(&shared.transcript_id) = started.transcript_id.clone();
                *Shared::lock(&shared.acp_session) = Some(started.acp_session_id.clone());
            }
            SessionEvent::Nori(NoriEvent::RequestWithdrawn(id)) => {
                Shared::lock(&shared.pending).retain(|pending| pending.request_id != *id);
            }
            SessionEvent::Acp(AcpEvent::Request {
                request_id,
                request: acp::AgentRequest::RequestPermissionRequest(request),
            }) => {
                let pending = PendingPermission {
                    request_id: request_id.clone(),
                    session_id: request.session_id.clone(),
                    tool: request.tool_call.fields.title.clone().unwrap_or_default(),
                    options: request
                        .options
                        .iter()
                        .map(|option| (option.option_id.to_string(), option.kind))
                        .collect(),
                    seq,
                };
                match shared.approvals.decide(&pending) {
                    Some(decision) => {
                        if let Err(error) =
                            respond(&handle, &shared, &pending, decision, "driver").await
                        {
                            tracing::warn!("could not answer a permission request: {error:#}");
                        }
                    }
                    None => Shared::lock(&shared.pending).push(pending),
                }
            }
            _ => {}
        }
    }
    shared.ended.store(true, Ordering::SeqCst);
    shared.changed.send_replace(-2);
}
