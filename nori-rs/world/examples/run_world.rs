//! Run a world through the real harness and print what happened.
//!
//!     cargo run -p nori-world --example run_world -- fake-agent/worlds/smoke.pkl "hello" [more prompts...]
//!     NORI_WORLD_APPROVALS=allow|deny|manual   (manual: approve every request when the turn settles)
//!     NORI_WORLD_AGENT=<slug> NORI_WORLD_QUIET_MS=<ms> NORI_WORLD_TIMEOUT_MS=<ms>
//!     NORI_WORLD_CLOCK=virtual|real|x10   (virtual by default: a turn that ends before a `/cancel` is over)
//!
//! Arguments after the world are a script: plain arguments are prompts; `/settle`, `/wait MS`,
//! `/cancel`, `/approve`, `/deny`, `/resume`, `/rewind [HEAD]` and `/stop-agent` are the user's other actions.
//!
//! The summary is the same one a test asserts on: the root's text, tool calls with their status,
//! subagents, permission requests, and the invariants that did not hold.

use nori_fake_agent::ClockMode;
use nori_protocol::NoriEvent;
use nori_world::Approvals;
use nori_world::Decision;
use nori_world::Events;
use nori_world::Stuck;
use nori_world::World;
use nori_world::WorldOptions;
use nori_world::WorldSession;

fn summarize(events: &Events) {
    let root = events.root();
    println!("--- root ---");
    println!("text: {:?}", root.text());
    if !root.thoughts().is_empty() {
        println!("thoughts: {:?}", root.thoughts());
    }
    for call in root.tool_calls() {
        println!(
            "tool {} [{:?}] {:?} statuses={:?} diffs={} text={:?}",
            call.id,
            call.kind,
            call.title,
            call.statuses,
            call.diffs.len(),
            call.text.chars().take(60).collect::<String>()
        );
    }
    if let Some(plan) = root.plan() {
        println!("plan: {plan:?}");
    }
    for report in root.usage_reports() {
        println!("usage: {report}");
    }
    for info in events.subagents() {
        let lane = events.child(&info.session_id);
        println!(
            "--- subagent {} agent={:?} title={:?} state={:?} ---",
            info.session_id, info.agent, info.title, info.state
        );
        println!("text: {:?}", lane.text());
        for call in lane.tool_calls() {
            println!("tool {} {:?} {:?}", call.id, call.title, call.statuses);
        }
    }
    for permission in events.permission_requests() {
        println!(
            "permission asked: {:?} {:?}",
            permission.title, permission.options
        );
    }
    for event in events.nori() {
        match event {
            NoriEvent::SessionEnded(ended) => println!("session ended: {ended:?}"),
            NoriEvent::Notice(notice) => println!("notice: {}", notice.message),
            NoriEvent::AgentContinued(continued) => println!("agent continued: {continued:?}"),
            NoriEvent::RequestDeclined(declined) => println!("request declined: {declined:?}"),
            _ => {}
        }
    }
    println!("last phase: {:?}", events.phase());
    for change in events.route_changes() {
        println!("route changed: {change:?}");
    }
    for failure in events.failures() {
        println!("request failed: {failure:?}");
    }
}

/// Settle; in manual mode approve whatever is waiting, as a person clicking Allow would.
async fn settle(session: &WorldSession, manual: bool) -> anyhow::Result<()> {
    loop {
        match session.settle().await {
            Ok(_) => return Ok(()),
            Err(Stuck::AwaitingUser(pending)) if manual => {
                for request in pending {
                    println!("(approving {:?})", request.tool);
                    session.answer(Decision::Allow).await?;
                }
            }
            Err(error) => {
                println!("stuck: {error}");
                return Ok(());
            }
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: run_world <world> <prompt>..."))?;
    let prompts: Vec<String> = args.collect();
    let world = World::from_path(std::path::Path::new(&path))?;
    let approvals = match std::env::var("NORI_WORLD_APPROVALS").as_deref() {
        Ok("allow") => Approvals::AllowAll,
        Ok("deny") => Approvals::DenyAll,
        _ => Approvals::Manual,
    };
    let manual = matches!(approvals, Approvals::Manual);
    let mut options = WorldOptions {
        approvals,
        ..WorldOptions::default()
    };
    options.agent = std::env::var("NORI_WORLD_AGENT").ok();
    if let Ok(clock) = std::env::var("NORI_WORLD_CLOCK") {
        options.clock = ClockMode::parse(&clock).map_err(|message| anyhow::anyhow!(message))?;
    }
    if let Some(millis) = std::env::var("NORI_WORLD_QUIET_MS")
        .ok()
        .and_then(|text| text.parse().ok())
    {
        options.quiet = std::time::Duration::from_millis(millis);
    }
    if let Some(millis) = std::env::var("NORI_WORLD_TIMEOUT_MS")
        .ok()
        .and_then(|text| text.parse().ok())
    {
        options.timeout = std::time::Duration::from_millis(millis);
    }
    let session = WorldSession::boot(world, options).await?;
    // Arguments are a small script: a plain argument is a prompt (submitted, not awaited), and
    // `/settle`, `/wait MS`, `/cancel`, `/approve`, `/deny`, `/resume`, `/stop-agent` are the
    // user's other actions. The script ends with a settle.
    for step in &prompts {
        println!("=== {step}");
        match step.as_str() {
            "/settle" => settle(&session, manual).await?,
            "/cancel" => session.cancel().await?,
            "/approve" => {
                let request = session.next_permission().await?;
                println!("(approving {:?})", request.tool);
                session.approve().await?;
            }
            "/deny" => {
                let request = session.next_permission().await?;
                println!("(denying {:?})", request.tool);
                session.deny().await?;
            }
            "/resume" => session.resume().await?,
            "/rewind" => session.rewind(None).await?,
            "/stop-agent" => session.stop_agent().await?,
            other if other.starts_with("/rewind ") => {
                session.rewind(Some(other[8..].trim().parse()?)).await?;
            }
            other => match other.strip_prefix("/wait ").map(str::parse::<u64>) {
                Some(Ok(millis)) => {
                    tokio::time::sleep(std::time::Duration::from_millis(millis)).await
                }
                Some(Err(error)) => anyhow::bail!("/wait takes milliseconds: {error}"),
                None => {
                    session.submit(other).await?;
                }
            },
        }
    }
    settle(&session, manual).await?;
    let report = session.finish().await?;
    summarize(&report.events);
    println!("--- recorder ({} entries) ---", report.recorder.len());
    for entry in &report.recorder {
        println!("{}", serde_json::to_string(entry)?);
    }
    println!("--- violations ---");
    for violation in &report.violations {
        println!("{violation}");
    }
    for divergence in &report.divergences {
        println!("divergence: {divergence}");
    }
    Ok(())
}
