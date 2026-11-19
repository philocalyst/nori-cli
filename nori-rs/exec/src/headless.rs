//! A headless frontend of the same turn ledger the interactive TUI drives.
//!
//! `nori exec` sends one prompt and then behaves as the TUI would if nobody touched the keyboard:
//! it keeps running while the session has work in flight. A turn the user never typed (a wake
//! with delegated results, delivered mail) streams like any other. It finishes only when the
//! root is idle and nothing is pending: no running tasks, no results owed, no mail queued. It
//! says plainly when it gives up waiting.
//!
//! The exit status is the truth about the *final* turn. It is success only if that turn
//! completed; a failed or cancelled turn, a lost connection, or a wait that ran out with work
//! pending is a failure with its cause on stderr. Nothing here prints a success for a failed
//! path.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use nori_config::NoriConfig;
use nori_harness::runtime::AgentPrepareSpec;
use nori_harness::runtime::HarnessHandle;
use nori_harness::runtime::SessionStart;
use nori_harness::runtime::prepare_and_launch_session;
use nori_installed::AnalyticsReporter;
use nori_protocol::AcpEvent;
use nori_protocol::MessageFlow;
use nori_protocol::MessageStream;
use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;
use nori_protocol::SubagentWire;
use nori_protocol::acp::v1 as acp;
use tokio::sync::mpsc::UnboundedReceiver;

/// How often the pending work is checked while waiting for it.
const POLL: Duration = Duration::from_millis(250);
/// Pending work must read idle this long, twice, before the run finishes: an event that is
/// still in flight (a wake being scheduled) must have had the chance to arrive.
const SETTLE: Duration = Duration::from_millis(400);

/// How to run.
#[derive(Debug, Clone, Default)]
pub struct HeadlessOptions {
    /// Give up waiting for pending work after this long once the root is idle. Without it,
    /// wait until nothing is pending.
    pub max_wait: Option<Duration>,
    /// Write every session event to stdout as one JSON object per line, and nothing else.
    pub json: bool,
}

/// Where output goes.
pub struct Sinks {
    pub stdout: Box<dyn Write + Send>,
    pub stderr: Box<dyn Write + Send>,
}

/// The end of a headless run.
#[derive(Debug, PartialEq, Eq)]
pub enum HeadlessOutcome {
    /// The final turn completed and nothing was left pending.
    Completed,
    /// The final turn did not complete, or the session was lost. The reason was printed.
    Failed(String),
    /// The wait ran out with work still pending. The message was printed.
    GaveUp(String),
}

/// Parse `--max-wait`: a number of seconds, or a number with `ms`, `s`, `m` or `h`.
pub fn parse_wait(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: f64 = number
        .parse()
        .map_err(|_| format!("`{text}` is not a duration; try 90s, 5m or 2h"))?;
    let seconds = match unit.trim() {
        "" | "s" | "sec" | "secs" => number,
        "ms" => number / 1000.0,
        "m" | "min" | "mins" => number * 60.0,
        "h" | "hr" | "hrs" => number * 3600.0,
        other => {
            return Err(format!(
                "unknown duration unit `{other}`; use ms, s, m or h"
            ));
        }
    };
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(format!("`{text}` is not a duration; try 90s, 5m or 2h"));
    }
    Ok(Duration::from_secs_f64(seconds))
}

/// How the last turn ended.
enum TurnEnd {
    Completed,
    Failed(String),
}

struct Run {
    json: bool,
    sinks: Sinks,
    last_turn: Option<TurnEnd>,
    session_ended: Option<String>,
    permission_denied: bool,
    /// The failure already printed, so the verdict does not print it twice.
    reported_failure: Option<String>,
    /// Nothing written to stdout since the last newline, or nothing written.
    at_line_start: bool,
    /// A message ended; the next answer text starts a new line.
    break_pending: bool,
    subagents: std::collections::HashMap<String, String>,
}

impl Run {
    fn out(&mut self, text: &str) -> std::io::Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        self.sinks.stdout.write_all(text.as_bytes())?;
        self.sinks.stdout.flush()?;
        self.at_line_start = text.ends_with('\n');
        Ok(())
    }

    fn err(&mut self, line: &str) {
        let _ = writeln!(self.sinks.stderr, "{line}");
        let _ = self.sinks.stderr.flush();
    }

    /// A line of status in the human stream: on its own line, never glued to answer text.
    fn out_line(&mut self, line: &str) -> std::io::Result<()> {
        if !self.at_line_start {
            self.out("\n")?;
        }
        self.out(&format!("{line}\n"))?;
        self.break_pending = false;
        Ok(())
    }

    fn answer(&mut self, text: &str) -> std::io::Result<()> {
        if self.break_pending && !self.at_line_start {
            self.out("\n")?;
        }
        self.break_pending = false;
        self.out(text)
    }
}

/// Run one prompt to the end of everything it set going.
pub async fn run_headless(
    config: Arc<NoriConfig>,
    cli_version: String,
    prompt: String,
    analytics: Option<AnalyticsReporter>,
    options: HeadlessOptions,
    sinks: Sinks,
) -> anyhow::Result<HeadlessOutcome> {
    let mut launched = prepare_and_launch_session(
        AgentPrepareSpec {
            handoff: None,
            config,
            cli_version,
            session_context: None,
            initial_context: None,
        },
        SessionStart::New,
    );
    if let Some(reporter) = analytics.as_ref() {
        launched.handle = reporter.attach(launched.handle);
    }
    let mut run = Run {
        json: options.json,
        sinks,
        last_turn: None,
        session_ended: None,
        permission_denied: false,
        reported_failure: None,
        at_line_start: true,
        break_pending: false,
        subagents: Default::default(),
    };
    let sent = launched
        .handle
        .prompt(vec![acp::ContentBlock::Text(acp::TextContent::new(prompt))])
        .await;
    let outcome = match sent {
        Ok(_) => drive(&launched.handle, &mut launched.events, &mut run, &options).await,
        Err(error) => Err(error),
    };
    let shutdown = launched.handle.shutdown().await;
    if let Some(reporter) = analytics {
        reporter.flush();
    }
    let outcome = outcome?;
    // A session that cannot be shut down cleanly is not a clean success.
    match (outcome, shutdown) {
        (HeadlessOutcome::Completed, Err(error)) => Ok(HeadlessOutcome::Failed(format!(
            "shutting down the session failed: {error:#}"
        ))),
        (outcome, _) => Ok(outcome),
    }
}

async fn drive(
    handle: &HarnessHandle,
    events: &mut UnboundedReceiver<SessionEvent>,
    run: &mut Run,
    options: &HeadlessOptions,
) -> anyhow::Result<HeadlessOutcome> {
    let mut idle_since: Option<tokio::time::Instant> = None;
    let mut waiting_since: Option<tokio::time::Instant> = None;
    loop {
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else {
                    return Ok(finish_lost(run, "the session ended unexpectedly"));
                };
                handle_event(handle, run, event).await?;
                if let Some(message) = run.session_ended.take() {
                    return Ok(finish_lost(run, &message));
                }
                // New activity restarts the idle check.
                idle_since = None;
            }
            () = tokio::time::sleep(POLL) => {}
        }
        // Nothing can be final before some turn has ended.
        if run.last_turn.is_none() {
            continue;
        }
        let pending = handle.pending_work().await?;
        if pending.is_idle() {
            let since = *idle_since.get_or_insert_with(tokio::time::Instant::now);
            if since.elapsed() >= SETTLE {
                // Whatever arrived while the check settled is part of the run.
                while let Ok(event) = events.try_recv() {
                    handle_event(handle, run, event).await?;
                }
                if let Some(message) = run.session_ended.take() {
                    return Ok(finish_lost(run, &message));
                }
                if handle.pending_work().await?.is_idle() {
                    return Ok(finish(run));
                }
                idle_since = None;
            }
            continue;
        }
        idle_since = None;
        let waiting = *waiting_since.get_or_insert_with(tokio::time::Instant::now);
        if let Some(max_wait) = options.max_wait
            && waiting.elapsed() >= max_wait
        {
            let message = format!("stopped waiting with {}", pending.describe());
            run.err(&format!("nori exec: {message}"));
            return Ok(HeadlessOutcome::GaveUp(message));
        }
    }
}

fn finish(run: &mut Run) -> HeadlessOutcome {
    if run.permission_denied {
        let message = "a permission request offered no allow option during unattended execution";
        run.err(&format!("nori exec: {message}"));
        return HeadlessOutcome::Failed(message.to_owned());
    }
    // A final newline for a plaintext answer that did not end with one.
    if !run.json && !run.at_line_start {
        let _ = run.out("\n");
    }
    match run.last_turn.take() {
        Some(TurnEnd::Completed) => HeadlessOutcome::Completed,
        Some(TurnEnd::Failed(reason)) => {
            if run.reported_failure.as_deref() != Some(reason.as_str()) {
                run.err(&format!("nori exec: {reason}"));
            }
            HeadlessOutcome::Failed(reason)
        }
        None => HeadlessOutcome::Failed("no turn completed".to_owned()),
    }
}

fn finish_lost(run: &mut Run, message: &str) -> HeadlessOutcome {
    if !run.json && !run.at_line_start {
        let _ = run.out("\n");
    }
    run.err(&format!("nori exec: {message}"));
    HeadlessOutcome::Failed(message.to_owned())
}

async fn handle_event(
    handle: &HarnessHandle,
    run: &mut Run,
    event: SessionEvent,
) -> anyhow::Result<()> {
    if run.json {
        let line = serde_json::to_string(&event)?;
        run.out(&format!("{line}\n"))?;
    }
    match event {
        SessionEvent::Acp(AcpEvent::Notification(acp::AgentNotification::SessionNotification(
            notification,
        ))) => {
            let flow = nori_protocol::update_flow(&notification.update);
            match (&notification.update, flow) {
                (
                    acp::SessionUpdate::AgentMessageChunk(chunk),
                    MessageFlow::Chunk(MessageStream::Answer) | MessageFlow::Boundary,
                ) => {
                    if let acp::ContentBlock::Text(text) = &chunk.content {
                        // A warning the backend carried in the text stream is not the answer.
                        let (prose, warning) =
                            match text.text.split_once(nori_protocol::AGENT_WARNING_PREFIX) {
                                Some((prose, warning)) => (prose, Some(warning)),
                                None => (text.text.as_str(), None),
                            };
                        if !run.json {
                            run.answer(prose)?;
                        }
                        if let Some(warning) = warning {
                            run.break_pending = true;
                            run.err(&format!("warning: {}", warning.trim()));
                        }
                    }
                }
                (_, MessageFlow::Boundary | MessageFlow::Chunk(_)) => run.break_pending = true,
                (_, MessageFlow::Transparent) => {}
            }
        }
        SessionEvent::Acp(AcpEvent::Request {
            request_id,
            request: acp::AgentRequest::RequestPermissionRequest(request),
        }) => {
            let response = crate::allow_permission(&request);
            run.permission_denied |=
                matches!(response.outcome, acp::RequestPermissionOutcome::Cancelled);
            handle
                .respond_to_agent(
                    request_id,
                    Ok(acp::ClientResponse::RequestPermissionResponse(response)),
                )
                .await?;
        }
        SessionEvent::Acp(AcpEvent::Request { request_id, .. }) => {
            handle
                .respond_to_agent(request_id, Err(acp::Error::method_not_found()))
                .await?;
        }
        SessionEvent::Acp(AcpEvent::Response {
            response: Ok(acp::AgentResponse::PromptResponse(response)),
            ..
        }) => {
            run.last_turn = Some(match response.stop_reason {
                acp::StopReason::EndTurn => TurnEnd::Completed,
                acp::StopReason::Cancelled => {
                    TurnEnd::Failed("the turn was cancelled before it finished".to_owned())
                }
                other => TurnEnd::Failed(format!("the turn stopped early: {other:?}")),
            });
            run.break_pending = true;
        }
        SessionEvent::Acp(_) => {}
        SessionEvent::Nori(event) => handle_nori(run, event)?,
    }
    Ok(())
}

fn handle_nori(run: &mut Run, event: NoriEvent) -> std::io::Result<()> {
    match event {
        NoriEvent::RequestFailed(failure) => {
            run.last_turn = Some(TurnEnd::Failed(failure.message.clone()));
            run.break_pending = true;
            run.err(&format!("error: {}", failure.message));
            run.reported_failure = Some(failure.message);
        }
        NoriEvent::SessionEnded(ended) => {
            run.session_ended = Some(ended.message.unwrap_or_else(|| {
                format!(
                    "the session ended ({:?}) before the work finished",
                    ended.reason
                )
            }));
        }
        NoriEvent::RouteChanged(changed) => {
            if !run.json {
                run.out_line(&format!(
                    "· continued on {} instead of {}: {}",
                    changed.to,
                    changed.from,
                    changed.cause.label()
                ))?;
            }
        }
        NoriEvent::RetryScheduled(retry) if !run.json => {
            run.err(&format!(
                "retrying in {}s (attempt {}/{}): {}",
                (retry.wait_ms + 999) / 1000,
                retry.attempt,
                retry.max_attempts,
                retry.class.label()
            ));
        }
        NoriEvent::Notice(notice) if !run.json => run.err(&format!("note: {}", notice.message)),
        NoriEvent::TaskResultsDelivered(delivered) if !run.json => {
            run.break_pending = true;
            let titles = delivered
                .tasks
                .iter()
                .map(|task| task.title.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            run.err(&format!("delegated results delivered: {titles}"));
        }
        NoriEvent::Monitor(nori_protocol::MonitorWire::Delivered(delivered)) if !run.json => {
            run.break_pending = true;
            for digest in delivered.monitors {
                run.err(&format!(
                    "monitor {}: {} events, {}",
                    digest.label, digest.events, digest.headline
                ));
            }
        }
        NoriEvent::Subagent(wire) if !run.json => subagent_line(run, &wire),
        NoriEvent::AgentContinued(continued) if !run.json => {
            run.err(&format!(
                "note: {} continued after its turn ended and asked to {}",
                continued.agent, continued.tool
            ));
        }
        _ => {}
    }
    Ok(())
}

/// One compact stderr line per change a delegated child reports.
fn subagent_line(run: &mut Run, wire: &SubagentWire) {
    match wire {
        SubagentWire::Update(update) => {
            let id = update.session_id.to_string();
            if let nori_protocol::Patch::Set(title) = &update.title {
                run.subagents.insert(id.clone(), title.clone());
            }
            let label = run
                .subagents
                .get(&id)
                .cloned()
                .unwrap_or_else(|| id.chars().take(8).collect());
            if let nori_protocol::Patch::Set(state) = &update.state {
                let state = match state {
                    nori_protocol::SubagentState::Running => "running".to_owned(),
                    nori_protocol::SubagentState::Idle => "idle".to_owned(),
                    nori_protocol::SubagentState::Ended(outcome) => {
                        format!("ended ({outcome:?})")
                    }
                };
                run.err(&format!("◈ {label}: {state}"));
            }
        }
        SubagentWire::Message(message) if !message.chunk => {
            let line = message.text.lines().next().unwrap_or_default();
            let line: String = line.chars().take(100).collect();
            run.err(&format!(
                "✉ {} → {}: {line}",
                message.sender_label.as_deref().unwrap_or("?"),
                message.recipient_label.as_deref().unwrap_or("?"),
            ));
        }
        SubagentWire::Message(_) => {}
    }
}
