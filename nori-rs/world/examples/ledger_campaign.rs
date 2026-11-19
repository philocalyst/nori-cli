//! An adversarial campaign against the turn ledger, through the real harness.
//!
//!     cargo run -p nori-world --example ledger_campaign [-- <scenario>...]
//!
//! Each scenario states the invariant it attacks and prints PASS or FAIL with what it saw.
//! Scenarios that need minutes of the harness's own time (silence) run on a paused tokio clock.

use std::time::Duration;

use nori_fake_agent::ClockMode;
use nori_protocol::NoriEvent;
use nori_protocol::SessionEndReason;
use nori_protocol::SessionPhase;
use nori_world::Approvals;
use nori_world::Decision;
use nori_world::Events;
use nori_world::Transport;
use nori_world::World;
use nori_world::WorldOptions;
use nori_world::WorldSession;

const CHAOS: &str = "fake-agent/worlds/adversarial-faults.pkl";
const CARBON: &str = "fake-agent/worlds/carbon-replay.pkl";

fn world(path: &str) -> anyhow::Result<World> {
    Ok(World::from_path(std::path::Path::new(path))?)
}

thread_local! {
    static TRANSPORT: std::cell::RefCell<Transport> = const { std::cell::RefCell::new(Transport::InProcess) };
}

fn transport() -> Transport {
    TRANSPORT.with(|t| t.borrow().clone())
}

fn options(clock: ClockMode) -> WorldOptions {
    WorldOptions {
        transport: transport(),
        clock,
        timeout: Duration::from_secs(40),
        quiet: Duration::from_millis(1200),
        ..WorldOptions::default()
    }
}

struct Row {
    transport: &'static str,
    scenario: String,
    invariant: String,
    pass: bool,
    detail: String,
}

fn row(
    rows: &mut Vec<Row>,
    scenario: &str,
    invariant: &str,
    pass: bool,
    detail: impl Into<String>,
) {
    let row = Row {
        transport: transport().label(),
        scenario: scenario.to_owned(),
        invariant: invariant.to_owned(),
        pass,
        detail: detail.into(),
    };
    println!(
        "{} | {} | {} | {} | {}",
        row.transport,
        row.scenario,
        row.invariant,
        if row.pass { "PASS" } else { "FAIL" },
        row.detail
    );
    rows.push(row);
}

fn last_idle(events: &Events) -> bool {
    matches!(events.phase(), Some(SessionPhase::Idle))
}

fn running_tools(events: &Events) -> Vec<String> {
    events
        .root()
        .tool_calls()
        .into_iter()
        .filter(|call| {
            matches!(
                call.status,
                nori_protocol::acp::v1::ToolCallStatus::Pending
                    | nori_protocol::acp::v1::ToolCallStatus::InProgress
            )
        })
        .map(|call| call.id)
        .collect()
}

fn count<F: Fn(&NoriEvent) -> bool>(events: &Events, f: F) -> usize {
    events.nori().filter(|event| f(event)).count()
}

async fn finish(session: WorldSession) -> anyhow::Result<Vec<String>> {
    let report = session.finish().await?;
    Ok(report
        .violations
        .iter()
        .map(ToString::to_string)
        .filter(|v| !v.starts_with("every_turn_ended"))
        .collect())
}

async fn crash(rows: &mut Vec<Row>) -> anyhow::Result<()> {
    let session = WorldSession::boot(world(CHAOS)?, options(ClockMode::Virtual)).await?;
    session.submit("crash").await?;
    let _ = session.settle().await;
    let events = session.events();
    row(
        rows,
        "crash mid-tool",
        "the open turn ends (phase Idle)",
        last_idle(&events),
        format!("{:?}", events.phase()),
    );
    row(
        rows,
        "crash mid-tool",
        "no tool call keeps running",
        running_tools(&events).is_empty(),
        format!("{:?}", running_tools(&events)),
    );
    let ended = events.nori().find_map(|e| match e {
        NoriEvent::SessionEnded(ended) => Some(ended.clone()),
        _ => None,
    });
    row(
        rows,
        "crash mid-tool",
        "SessionEnded(ConnectionLost) carries the cause",
        ended.as_ref().is_some_and(|e| {
            e.reason == SessionEndReason::ConnectionLost
                && e.message.as_deref().is_some_and(|m| !m.is_empty())
        }),
        format!("{ended:?}"),
    );
    let violations = finish(session).await?;
    row(
        rows,
        "crash mid-tool",
        "no harness invariant violated",
        violations.is_empty(),
        format!("{violations:?}"),
    );
    Ok(())
}

async fn dropped(rows: &mut Vec<Row>) -> anyhow::Result<()> {
    let session = WorldSession::boot(world(CHAOS)?, options(ClockMode::Virtual)).await?;
    session.submit("drop").await?;
    let _ = session.settle().await;
    let events = session.events();
    row(
        rows,
        "dropped response",
        "not auto-failed: still Prompting, no failure",
        matches!(events.phase(), Some(SessionPhase::Prompting { .. }))
            && events.failures().is_empty(),
        format!("{:?}", events.phase()),
    );
    session.cancel().await?;
    let waited = session
        .wait_for("the cancel to end the turn", |events| last_idle(events))
        .await;
    let events = session.events();
    row(
        rows,
        "dropped response",
        "cancel reaches an honest end (Idle, explicit failure)",
        waited.is_ok() && !events.failures().is_empty(),
        format!("{:?} failures={}", events.phase(), events.failures().len()),
    );
    let violations = finish(session).await?;
    row(
        rows,
        "dropped response",
        "no harness invariant violated",
        violations.is_empty(),
        format!("{violations:?}"),
    );
    Ok(())
}

async fn stale_and_twice(rows: &mut Vec<Row>) -> anyhow::Result<()> {
    for (name, prompts) in [
        ("stale response", vec!["stale", "stale"]),
        ("duplicate response", vec!["twice", "twice"]),
    ] {
        let session = WorldSession::boot(world(CHAOS)?, options(ClockMode::Virtual)).await?;
        for prompt in &prompts {
            let _ = session.ask(prompt).await;
        }
        let _ = session.settle().await;
        let events = session.events();
        let phases = events.phases();
        let prompting = phases
            .iter()
            .filter(|p| matches!(p, SessionPhase::Prompting { .. }))
            .count();
        row(
            rows,
            name,
            "each prompt is one turn: one Prompting phase per prompt",
            prompting == prompts.len(),
            format!(
                "prompting phases={prompting} of {}: {:?}",
                prompts.len(),
                phases
                    .iter()
                    .map(|p| match p {
                        SessionPhase::Idle => "idle".to_owned(),
                        SessionPhase::Prompting { request_id } => format!("prompting:{request_id}"),
                        other => format!("{other:?}"),
                    })
                    .collect::<Vec<_>>()
            ),
        );
        row(
            rows,
            name,
            "ends Idle, no failure invented",
            last_idle(&events) && events.failures().is_empty(),
            format!("{:?} failures={}", events.phase(), events.failures().len()),
        );
        let violations = finish(session).await?;
        row(
            rows,
            name,
            "no harness invariant violated",
            violations.is_empty(),
            format!("{violations:?}"),
        );
    }
    Ok(())
}

async fn detach_then_idle(rows: &mut Vec<Row>) -> anyhow::Result<()> {
    let mut opts = options(ClockMode::Virtual);
    opts.approvals = Approvals::Manual;
    let session = WorldSession::boot(world(CHAOS)?, opts).await?;
    session.submit("detach").await?;
    let request = session.next_permission().await;
    let events = session.events();
    let agent_phase = events
        .phases()
        .iter()
        .any(|p| matches!(p, SessionPhase::Agent { .. }));
    row(
        rows,
        "detach then permission",
        "the agent's continued work is an Agent turn, not dropped",
        agent_phase,
        format!(
            "phases={:?}",
            events
                .phases()
                .iter()
                .map(|p| format!("{p:?}"))
                .collect::<Vec<_>>()
        ),
    );
    let continued = count(
        &events,
        |e| matches!(e, NoriEvent::AgentContinued(c) if !c.after_cancel),
    );
    row(
        rows,
        "detach then permission",
        "AgentContinued names the agent and tool before the request",
        request.is_ok() && continued >= 1,
        format!("continued={continued}"),
    );
    // A prompt typed while the agent turn is open waits for it.
    // `prompt` returns once the prompt starts, which is after the agent turn ends.
    let (submitted, approved) = tokio::join!(session.submit("hello again"), async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        session.approve().await
    });
    submitted?;
    approved?;
    let _ = session.settle().await;
    let events = session.events();
    row(
        rows,
        "detach then idle",
        "the Agent turn ends, the typed prompt waited for it and then ran; ends Idle",
        last_idle(&events) && events.root().text().contains("last thing"),
        format!(
            "{:?} text={:?}",
            events.phase(),
            events.root().text().chars().take(120).collect::<String>()
        ),
    );
    let violations = finish(session).await?;
    row(
        rows,
        "detach then idle",
        "no harness invariant violated",
        violations.is_empty(),
        format!("{violations:?}"),
    );
    Ok(())
}

async fn interleave(rows: &mut Vec<Row>) -> anyhow::Result<()> {
    let session = WorldSession::boot(world(CHAOS)?, options(ClockMode::Virtual)).await?;
    session.submit("interleave").await?;
    let _ = session.settle().await;
    let events = session.events();
    let text = events.root().text();
    row(
        rows,
        "interleaved sessions",
        "the root's text is only the root's",
        text.contains("A1") && !text.contains("B1") && !text.contains("B2"),
        format!("{text:?}"),
    );
    row(
        rows,
        "interleaved sessions",
        "updates of an unknown session open no turn; ends Idle",
        last_idle(&events),
        format!("{:?}", events.phase()),
    );
    let violations = finish(session).await?;
    row(
        rows,
        "interleaved sessions",
        "no harness invariant violated",
        violations.is_empty(),
        format!("{violations:?}"),
    );
    for scene in ["noise", "disorder"] {
        let session = WorldSession::boot(world(CHAOS)?, options(ClockMode::Virtual)).await?;
        session.submit(scene).await?;
        let _ = session.settle().await;
        let events = session.events();
        row(
            rows,
            scene,
            "ends Idle with no ghost running tool",
            last_idle(&events) && running_tools(&events).is_empty(),
            format!("{:?} running={:?}", events.phase(), running_tools(&events)),
        );
        let violations = finish(session).await?;
        row(
            rows,
            scene,
            "no harness invariant violated",
            violations.is_empty(),
            format!("{violations:?}"),
        );
    }
    Ok(())
}

async fn cancel_races(rows: &mut Vec<Row>) -> anyhow::Result<()> {
    for (path, prompt) in [
        (CARBON, "cancel"),
        (CHAOS, "ignore"),
        (CHAOS, "detach"),
        (CHAOS, "hang"),
    ] {
        for delay in [0u64, 30, 120, 400, 1200] {
            let session = WorldSession::boot(
                world(path)?,
                options(ClockMode::parse("x10").map_err(anyhow::Error::msg)?),
            )
            .await?;
            session.submit(prompt).await?;
            tokio::time::sleep(Duration::from_millis(delay)).await;
            session.cancel().await?;
            if delay == 120 {
                session.cancel().await?; // double cancel
            }
            // Deny what the agent asks after being cancelled; stop waiting once it is Idle.
            let waited = session
                .wait_for("idle after cancel", |events| last_idle(events))
                .await;
            let _ = session.settle().await;
            // Answer anything left so nothing waits on the user.
            while session.pending_permissions().first().is_some() {
                let _ = session.answer(Decision::Deny).await;
                let _ = session.settle().await;
            }
            let events = session.events();
            let name = format!("cancel race {prompt} @{delay}ms");
            let _ = waited;
            row(
                rows,
                &name,
                "reaches Idle with no ghost tool (honest end)",
                last_idle(&events) && running_tools(&events).is_empty(),
                format!("{:?} running={:?}", events.phase(), running_tools(&events)),
            );
            let violations = finish(session).await?;
            row(
                rows,
                &name,
                "no harness invariant violated",
                violations.is_empty(),
                format!("{violations:?}"),
            );
        }
    }
    Ok(())
}

async fn rewind_resume(rows: &mut Vec<Row>) -> anyhow::Result<()> {
    // Rewind mid-turn: cancels cleanly, waits, then swaps.
    let session = WorldSession::boot(
        world(CARBON)?,
        options(ClockMode::parse("x10").map_err(anyhow::Error::msg)?),
    )
    .await?;
    session.submit("cancel").await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let rewound = session.rewind(None).await;
    let _ = session.settle().await;
    let events = session.events();
    row(
        rows,
        "rewind mid-turn",
        "cancels cleanly then rewinds (or refuses with a reason); never forks mid-turn",
        last_idle(&events) && running_tools(&events).is_empty(),
        format!(
            "rewind={:?} phase={:?}",
            rewound.as_ref().map_err(ToString::to_string),
            events.phase()
        ),
    );
    let violations = finish(session).await?;
    row(
        rows,
        "rewind mid-turn",
        "no harness invariant violated",
        violations.is_empty(),
        format!("{violations:?}"),
    );

    // Resume mid-turn.
    let session = WorldSession::boot(
        world(CARBON)?,
        options(ClockMode::parse("x10").map_err(anyhow::Error::msg)?),
    )
    .await?;
    session.submit("cancel").await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let resumed = session.resume().await;
    let _ = session.settle().await;
    let events = session.events();
    let after = events.phase();
    row(
        rows,
        "resume mid-turn",
        "the resumed session shows no running turn and no unanswerable permission",
        resumed.is_ok()
            && matches!(after, Some(SessionPhase::Idle))
            && session.pending_permissions().is_empty(),
        format!(
            "resume={:?} phase={after:?} pending={}",
            resumed.as_ref().map_err(ToString::to_string),
            session.pending_permissions().len()
        ),
    );
    row(
        rows,
        "resume mid-turn",
        "the unanswered prompt is offered back, tools closed",
        count(&events, |e| matches!(e, NoriEvent::PromptUnanswered(_))) >= 1
            || running_tools(&events).is_empty(),
        format!(
            "unanswered={} running={:?}",
            count(&events, |e| matches!(e, NoriEvent::PromptUnanswered(_))),
            running_tools(&events)
        ),
    );
    let tail: Vec<String> = events
        .nori()
        .filter_map(|e| match e {
            NoriEvent::Notice(n) => Some(format!("notice:{}", n.message)),
            NoriEvent::SessionEnded(s) => Some(format!("ended:{s:?}")),
            NoriEvent::RequestFailed(f) => Some(format!("failed:{}", f.message)),
            _ => None,
        })
        .collect();
    // A served process keeps its sessions in memory: a fresh one does not know the session. The
    // resume then ends with an honest cause, which is the right answer, not a hang.
    let honest_end = events.nori().any(|e| {
        matches!(e, NoriEvent::SessionEnded(s) if s.reason == SessionEndReason::SpawnFailed
            && s.message.as_deref().is_some_and(|m| m.contains("unknown session")))
    });
    match finish(session).await {
        Ok(violations) => row(
            rows,
            "resume mid-turn",
            "no harness invariant violated",
            violations.is_empty(),
            format!("{violations:?}"),
        ),
        Err(error) => row(
            rows,
            "resume mid-turn",
            "the agent cannot resume the session: the end is honest (SpawnFailed with the cause)",
            honest_end,
            format!("{error:#}; events: {tail:?}"),
        ),
    }
    Ok(())
}

const PROCESS: &str = "fake-agent/worlds/process-faults.pkl";

/// Whether a process started for the world in `home` is still alive.
fn leftover_process(home: &std::path::Path) -> bool {
    std::process::Command::new("pgrep")
        .arg("-f")
        .arg(home.display().to_string())
        .output()
        .is_ok_and(|output| output.status.success() && !output.stdout.is_empty())
}

async fn process_only(rows: &mut Vec<Row>) -> anyhow::Result<()> {
    for (keyword, what) in [
        ("exit0", "process exit, code 0, mid-turn"),
        ("exit3", "process exit, code 3, mid-turn"),
        ("sigkill", "SIGKILL mid-turn"),
        ("partial", "a partial JSON line, then exit"),
        ("closeout", "stdout closed while the process lives"),
    ] {
        let session = WorldSession::boot(world(PROCESS)?, options(ClockMode::Virtual)).await?;
        session.submit(keyword).await?;
        let waited = session
            .wait_for("the session to end", |events| {
                events
                    .nori()
                    .any(|e| matches!(e, NoriEvent::SessionEnded(_)))
            })
            .await;
        let events = session.events();
        let ended = events.nori().find_map(|e| match e {
            NoriEvent::SessionEnded(ended) => Some(ended.clone()),
            _ => None,
        });
        row(
            rows,
            what,
            "the open turn ends: the session says so (SessionEnded ConnectionLost with a cause), nothing hangs",
            waited.is_ok()
                && ended.as_ref().is_some_and(|e| {
                    e.reason == SessionEndReason::ConnectionLost
                        && e.message.as_deref().is_some_and(|m| !m.is_empty())
                }),
            format!("{:?}", ended),
        );
        row(
            rows,
            what,
            "ends Idle, no ghost running tool",
            last_idle(&events) && running_tools(&events).is_empty(),
            format!("{:?} running={:?}", events.phase(), running_tools(&events)),
        );
        let message = ended.and_then(|e| e.message).unwrap_or_default();
        let expect = match keyword {
            "exit0" => message.contains("exit code 0") || message.contains("code 0"),
            "exit3" => message.contains("exit code 3") || message.contains("code 3"),
            _ => true,
        };
        row(
            rows,
            what,
            "the cause names how the process ended",
            expect,
            message.replace('\n', " / "),
        );
        let home = session
            .nori_home()
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_default();
        let violations = finish(session).await?;
        tokio::time::sleep(Duration::from_millis(600)).await;
        row(
            rows,
            what,
            "no harness invariant violated, no leftover process",
            violations.is_empty() && !leftover_process(&home),
            format!("{violations:?} leftover={}", leftover_process(&home)),
        );
    }
    // stderr flood: the turn must complete.
    let session = WorldSession::boot(world(PROCESS)?, options(ClockMode::Virtual)).await?;
    let asked = session.ask("flood").await;
    let events = session.events();
    row(
        rows,
        "stderr flood (9 MB)",
        "the turn completes: nobody deadlocks on a full stderr pipe",
        asked.is_ok() && events.root().text().contains("Survived the flood") && last_idle(&events),
        format!("{:?} text={:?}", events.phase(), events.root().text()),
    );
    let violations = finish(session).await?;
    row(
        rows,
        "stderr flood (9 MB)",
        "no harness invariant violated",
        violations.is_empty(),
        format!("{violations:?}"),
    );
    // slow drain
    let session = WorldSession::boot(world(PROCESS)?, options(ClockMode::Virtual)).await?;
    let asked = session.ask("slow").await;
    let events = session.events();
    let text = events.root().text();
    row(
        rows,
        "slow stdout drain",
        "all output arrives, in order, none truncated",
        asked.is_ok()
            && text.contains("Line 1:")
            && text.contains("Line 8000:")
            && text.contains("END of slow output"),
        format!("{} bytes", text.len()),
    );
    row(
        rows,
        "slow stdout drain",
        "ends Idle",
        last_idle(&events),
        format!("{:?}", events.phase()),
    );
    let violations = finish(session).await?;
    row(
        rows,
        "slow stdout drain",
        "no harness invariant violated",
        violations.is_empty(),
        format!("{violations:?}"),
    );
    // cancel while backpressured
    let session = WorldSession::boot(world(PROCESS)?, options(ClockMode::Virtual)).await?;
    session.submit("slow").await?;
    tokio::time::sleep(Duration::from_millis(800)).await;
    session.cancel().await?;
    let waited = session
        .wait_for("cancel to end the turn", |events| last_idle(events))
        .await;
    let events = session.events();
    row(
        rows,
        "cancel under backpressure",
        "cancel is honoured while the pipe is full: reaches Idle",
        waited.is_ok() && last_idle(&events),
        format!("{:?} bytes={}", events.phase(), events.root().text().len()),
    );
    let violations = finish(session).await?;
    row(
        rows,
        "cancel under backpressure",
        "no harness invariant violated",
        violations.is_empty(),
        format!("{violations:?}"),
    );
    Ok(())
}

fn silence(rows: &mut Vec<Row>) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()?;
    runtime.block_on(async {
        let silent = World::from_path(std::path::Path::new(
            "/private/tmp/claude-501/-Users-philocalyst-Projects-nori-cli-nori-cli-nori-cli/308a79d0-f12c-4fba-a348-dd921f5fa7e7/scratchpad/worlds/silent.pkl",
        ))?;
        let mut opts = options(ClockMode::Real(1.0));
        opts.timeout = Duration::from_secs(3600);
        opts.quiet = Duration::from_secs(5);
        let session = WorldSession::boot(silent, opts).await?;
        session.submit("silent").await?;
        tokio::time::sleep(Duration::from_secs(60 * 4)).await;
        let early = count(&session.events(), |e| matches!(e, NoriEvent::AgentSilent(_)));
        row(rows, "silent agent", "no silence reported before five minutes", early == 0, format!("{early}"));
        tokio::time::sleep(Duration::from_secs(150)).await;
        let events = session.events();
        let silent = count(&events, |e| matches!(e, NoriEvent::AgentSilent(_)));
        row(rows, "silent agent", "AgentSilent is published after five minutes with a prompt outstanding; nothing is failed", silent >= 1 && events.failures().is_empty() && matches!(events.phase(), Some(SessionPhase::Prompting { .. })), format!("silent={silent} phase={:?}", events.phase()));
        tokio::time::sleep(Duration::from_secs(60 * 7)).await;
        let events = session.events();
        let heard = count(&events, |e| matches!(e, NoriEvent::AgentHeard));
        row(rows, "silent agent", "any activity clears the silence (AgentHeard) and the turn completes", heard >= 1 && events.root().text().contains("back"), format!("heard={heard} text={:?}", events.root().text()));
        let _ = session.settle().await;
        let violations = finish(session).await?;
        row(rows, "silent agent", "no harness invariant violated", violations.is_empty(), format!("{violations:?}"));
        Ok::<_, anyhow::Error>(())
    })
}

async fn campaign(rows: &mut Vec<Row>, wanted: &[String]) -> anyhow::Result<()> {
    let run = |name: &str| wanted.is_empty() || wanted.iter().any(|w| w == name);
    if run("crash") {
        crash(rows).await?;
    }
    if run("dropped") {
        dropped(rows).await?;
    }
    if run("stale") {
        stale_and_twice(rows).await?;
    }
    if run("detach") {
        detach_then_idle(rows).await?;
    }
    if run("interleave") {
        interleave(rows).await?;
    }
    if run("cancel") {
        cancel_races(rows).await?;
    }
    if run("rewind") {
        rewind_resume(rows).await?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut transports = vec![Transport::InProcess];
    if let Some(index) = args.iter().position(|a| a.starts_with("--nori=")) {
        let nori = args.remove(index).trim_start_matches("--nori=").to_owned();
        transports.push(Transport::Process { nori: nori.into() });
    }
    if let Some(index) = args.iter().position(|a| a == "--only-process") {
        args.remove(index);
        transports.remove(0);
    }
    let wanted = args;
    let mut rows = Vec::new();
    for t in transports {
        TRANSPORT.with(|cell| *cell.borrow_mut() = t.clone());
        campaign(&mut rows, &wanted).await?;
        if matches!(t, Transport::Process { .. })
            && (wanted.is_empty() || wanted.iter().any(|w| w == "process"))
        {
            process_only(&mut rows).await?;
        }
    }
    if wanted.is_empty() || wanted.iter().any(|w| w == "silence") {
        TRANSPORT.with(|cell| *cell.borrow_mut() = Transport::InProcess);
        // A paused clock needs a runtime of its own.
        let handle = std::thread::spawn(move || {
            TRANSPORT.with(|cell| *cell.borrow_mut() = Transport::InProcess);
            let mut inner = Vec::new();
            let result = silence(&mut inner);
            (inner, result)
        });
        let (inner, result) = handle
            .join()
            .map_err(|_| anyhow::anyhow!("silence scenario panicked"))?;
        rows.extend(inner);
        result?;
    }
    let failed = rows.iter().filter(|r| !r.pass).count();
    println!("\n{} checks, {} failed", rows.len(), failed);
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}
