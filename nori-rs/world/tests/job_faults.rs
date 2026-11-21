//! What Nori does with a background job that is not the happy case, on scripted processes: a job
//! that never finishes, floods, dies, or is not there.
//!
//! World: `fake-agent/worlds/jobs.pkl`. Jobs run the world's `processes` instead of anything real
//! (`world/src/processes.rs`), so each test is the same every run; the supervisor, the registry, the
//! wakes and the tools are the harness's own. Process time is real time (see `processes.rs`), a few
//! seconds a test. See `docs/fault-catalog.md`.

mod support;

use nori_protocol::MonitorWire;
use nori_protocol::NoriEvent;
use nori_world::Events;
use nori_world::Record;
use nori_world::WorldOptions;
use nori_world::WorldSession;
use pretty_assertions::assert_eq;

const WORLD: &str = "jobs.pkl";

/// Boots the lead agent. These tests share one agent slug, so they run one at a time and each
/// plays seconds of real process time: a late one waits longer for its turn than the default.
async fn boot(world_file: &str, agent: &str) -> WorldSession {
    let options = WorldOptions {
        timeout: std::time::Duration::from_secs(300),
        ..support::options(agent)
    };
    WorldSession::boot(support::world(world_file), options)
        .await
        .unwrap_or_else(|error| panic!("boot {agent}: {error:#}"))
}

/// The prompts the lead agent was handed that were monitor wakes.
fn wakes(session: &WorldSession) -> Vec<String> {
    session
        .recorder()
        .iter()
        .filter_map(|entry| match &entry.record {
            Record::PromptReceived { agent, text, .. }
                if agent == "lead" && text.contains("<monitor_events") =>
            {
                Some(text.clone())
            }
            _ => None,
        })
        .collect()
}

/// What each call of host tool `tool` returned, oldest first: its output, and whether it failed.
fn calls(session: &WorldSession, tool: &str) -> Vec<(String, bool)> {
    session
        .recorder()
        .iter()
        .filter_map(|entry| match &entry.record {
            Record::HostTool {
                name,
                output,
                is_error,
                ..
            } if name == tool => Some((output.clone(), *is_error)),
            _ => None,
        })
        .collect()
}

/// What each `job_read` returned, oldest first.
fn reads(session: &WorldSession) -> Vec<serde_json::Value> {
    calls(session, "job_read")
        .iter()
        .filter_map(|(output, _)| serde_json::from_str(output).ok())
        .collect()
}

/// The running-jobs count of each `MonitorWire::State` published, with repeats collapsed.
fn job_counts(events: &Events) -> Vec<i64> {
    let mut counts: Vec<i64> = events
        .nori()
        .filter_map(|event| match event {
            NoriEvent::Monitor(MonitorWire::State(state)) => Some(state.jobs),
            _ => None,
        })
        .collect();
    counts.dedup();
    counts
}

fn woken_by(session: &WorldSession, text: &str) -> bool {
    wakes(session).iter().any(|wake| wake.contains(text))
}

/// FAULT: job-ready-exit. A job that says it is ready and then exits 3 wakes the agent twice, once
/// for each, and `job_read` returns each line exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_wakes_the_agent_once_when_ready_and_once_when_it_exits() {
    let session = boot(WORLD, "lead").await;
    session.ask("run ready").await.expect("the starting turn");
    session
        .wait_for("the ready wake", |_| woken_by(&session, "job ready: READY"))
        .await
        .expect("the ready line is delivered");
    session
        .wait_for("the exit wake", |_| woken_by(&session, "exited 3"))
        .await
        .expect("the exit is delivered");
    session.settle().await.expect("the wake turns end");
    let wakes = wakes(&session);
    assert_eq!(wakes.len(), 2, "{wakes:#?}");
    assert!(
        wakes[0].contains("job ready: READY") && !wakes[0].contains("exited"),
        "{wakes:#?}"
    );
    assert!(
        wakes[1].contains("exited 3") && !wakes[1].contains("job ready"),
        "{wakes:#?}"
    );

    session.ask("read boot").await.expect("the reading turn");
    let reads = reads(&session);
    assert_eq!(reads.len(), 2, "{reads:#?}");
    assert_eq!(reads[0]["stdout"], "booting\nREADY", "{reads:#?}");
    assert_eq!(reads[0]["state"], "exited 3", "{reads:#?}");
    assert_eq!(reads[0]["lines"], 2, "{reads:#?}");
    assert_eq!(
        reads[1].get("stdout"),
        None,
        "nothing is read twice: {reads:#?}"
    );
    assert_eq!(reads[1]["lines"], 0, "{reads:#?}");
    assert_eq!(session.divergences(), Vec::<String>::new());
}

/// FAULT: job-never-ends. A job that runs until stopped never wakes the agent; the agent stops it,
/// the stop it asked for is not news, and the list says it was killed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_that_never_finishes_wakes_nobody_and_is_stopped_quietly() {
    let session = boot(WORLD, "lead").await;
    session.ask("run forever").await.expect("the starting turn");
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert_eq!(wakes(&session), Vec::<String>::new());

    session
        .ask("stop forever")
        .await
        .expect("the stopping turn");
    let stops = calls(&session, "job_stop");
    assert_eq!(stops.len(), 1, "{stops:#?}");
    assert!(!stops[0].1, "the stop worked: {stops:#?}");
    let stopped: serde_json::Value = serde_json::from_str(&stops[0].0).expect("job_stop's JSON");
    assert_eq!(stopped["state"], "killed by TERM", "{stopped}");
    assert_eq!(
        wakes(&session),
        Vec::<String>::new(),
        "a stop it asked for is not news"
    );

    session.ask("list jobs").await.expect("the listing turn");
    let listed = calls(&session, "job_list");
    let listed: serde_json::Value = serde_json::from_str(&listed[0].0).expect("job_list's JSON");
    let states: Vec<(&str, &str)> = listed["jobs"]
        .as_array()
        .expect("a list")
        .iter()
        .map(|job| {
            (
                job["label"].as_str().unwrap_or_default(),
                job["state"].as_str().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(states, vec![("forever", "killed by TERM")]);
    assert_eq!(wakes(&session), Vec::<String>::new());
}

/// FAULT: job-flood. Three thousand lines in three seconds, twenty of them matching
/// `wake_on_output`: the matches reach the agent in fewer, larger wakes, each once; the session
/// answers a prompt meanwhile; and a read with a small `max_bytes` returns at most that and says
/// there is more.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_that_floods_output_is_batched_and_the_session_stays_responsive() {
    let session = boot(WORLD, "lead").await;
    session
        .submit("run flood")
        .await
        .expect("the starting turn");
    session
        .wait_for("the first wake", |_| woken_by(&session, "ERROR round"))
        .await
        .expect("a match wakes the agent");

    let ping = session
        .submit("ping")
        .await
        .expect("a prompt during the flood");
    let answered = session
        .wait_for("the answer", |events| {
            events.responses().iter().any(|(id, _)| *id == ping)
        })
        .await
        .expect("the session answers while the job floods");
    assert!(
        answered.root().text().contains("pong"),
        "{:?}",
        answered.root().text()
    );
    let early: usize = wakes(&session)
        .iter()
        .map(|wake| wake.matches("ERROR round").count())
        .sum();
    assert!(
        early < 20,
        "the flood was still going when it answered: {early} matches so far"
    );

    // Six wakes in a minute are a storm: what follows is gathered into one digest 30 seconds on.
    session
        .wait_for("the exit wake", |_| {
            woken_by(&session, "The command exited 0")
        })
        .await
        .expect("the end is delivered");
    session.settle().await.expect("the wake turns end");
    let wakes = wakes(&session);
    assert!(
        wakes.len() < 20,
        "twenty matches are batched, not twenty wakes: {} wakes",
        wakes.len()
    );
    for round in 1..=20 {
        let line = format!("ERROR round {round} failed");
        let seen: usize = wakes.iter().map(|wake| wake.matches(&line).count()).sum();
        assert_eq!(seen, 1, "`{line}` is delivered once");
    }
    assert!(
        wakes.iter().all(|wake| !wake.contains("noise")),
        "lines that do not match are not wakes"
    );

    session.ask("read flood").await.expect("the reading turn");
    let reads = reads(&session);
    assert_eq!(reads.len(), 2, "{reads:#?}");
    let lines = |read: &serde_json::Value| -> Vec<String> {
        read["stdout"]
            .as_str()
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    };
    let (first, second) = (lines(&reads[0]), lines(&reads[1]));
    for (read, chunk) in reads.iter().zip([&first, &second]) {
        let bytes: usize = chunk.iter().map(|line| line.len() + 1).sum();
        assert!(bytes <= 8192, "a read of 8192 bytes returned {bytes}");
        assert!(!chunk.is_empty(), "{read}");
        assert_eq!(read["more"], true, "output is left behind: {read}");
    }
    assert!(
        first.iter().all(|line| !second.contains(line)),
        "the second read starts where the first ended"
    );
    assert_eq!(session.divergences(), Vec::<String>::new());
}

/// FAULT: job-dies. A job that prints a line on stderr and exits 1 wakes the agent with the failure
/// and the line it died saying.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_that_dies_wakes_the_agent_with_its_last_words() {
    let session = boot(WORLD, "lead").await;
    session.ask("run dying").await.expect("the starting turn");
    session
        .wait_for("the exit wake", |_| woken_by(&session, "exited 1"))
        .await
        .expect("the failure is delivered");
    session.settle().await.expect("the wake turn ends");
    let wakes = wakes(&session);
    assert_eq!(wakes.len(), 1, "{wakes:#?}");
    assert!(wakes[0].contains("that is a failure"), "{wakes:#?}");
    assert!(
        wakes[0].contains("fatal: the database is gone"),
        "the stderr line comes with the failure: {wakes:#?}"
    );
}

/// FAULT: job-unknown-command. A command no process of the world matches does not start, says why
/// in the tool's error, and leaves nothing listed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_command_the_world_runs_nothing_for_fails_to_start() {
    let session = boot(WORLD, "lead").await;
    session.ask("run nothing").await.expect("the starting turn");
    let starts = calls(&session, "job_start");
    assert_eq!(starts.len(), 1, "{starts:#?}");
    assert!(starts[0].1, "the start failed: {starts:#?}");
    assert!(
        starts[0]
            .0
            .contains("this world runs no process for \"unknown-program --flag\""),
        "{starts:#?}"
    );

    session.ask("list jobs").await.expect("the listing turn");
    let listed = calls(&session, "job_list");
    let listed: serde_json::Value = serde_json::from_str(&listed[0].0).expect("job_list's JSON");
    assert_eq!(listed["jobs"], serde_json::json!([]));
    assert_eq!(
        job_counts(&session.events())
            .iter()
            .max()
            .copied()
            .unwrap_or(0),
        0
    );
    assert_eq!(session.divergences(), Vec::<String>::new());
}

/// FAULT: job-count. The count the status line shows follows the jobs: one when a job runs, two
/// with another, one when that one ends, none when the last is stopped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_running_jobs_count_follows_the_jobs() {
    let session = boot(WORLD, "lead").await;
    session.ask("run forever").await.expect("the first job");
    assert_eq!(job_counts(&session.events()), vec![1]);
    session.submit("run short").await.expect("the second job");
    session
        .wait_for("the second job to be counted", |events| {
            job_counts(events).contains(&2)
        })
        .await
        .expect("two jobs are counted");
    session
        .wait_for("the second job to end", |events| {
            job_counts(events) == vec![1, 2, 1]
        })
        .await
        .expect("an ended job is not counted");
    session.settle().await.expect("nothing else runs");
    session
        .ask("stop forever")
        .await
        .expect("the stopping turn");
    session
        .wait_for("no jobs", |events| job_counts(events) == vec![1, 2, 1, 0])
        .await
        .expect("a stopped job is not counted");
}

/// FAULT: job-instant-ready. A job that prints its ready line the moment it starts still wakes the
/// agent for it: the line is not lost between the start and the watch that follows it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_ready_line_printed_at_once_is_not_lost() {
    let session = boot(WORLD, "lead").await;
    session.ask("run instant").await.expect("the starting turn");
    session
        .wait_for("the ready wake", |_| woken_by(&session, "job ready: READY"))
        .await
        .expect("the ready line is delivered");
}
