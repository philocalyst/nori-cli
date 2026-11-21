//! Background jobs through the real harness and the real local supervisor: a job wakes the agent
//! when it is ready and when it exits, and `job_read` returns each line once.
//!
//! World: `fake-agent/worlds/monitors.pkl` (`run the job`, `read the job output`). The job's
//! files live under `~/.nori/jobs/<id>`, as for any job.

mod support;

use nori_protocol::MonitorWire;
use nori_protocol::NoriEvent;
use nori_world::Events;
use nori_world::Record;
use support::boot;

const WORLD: &str = "monitors.pkl";

/// The prompts the lead agent was handed that were monitor wakes.
fn wakes(session: &nori_world::WorldSession) -> Vec<String> {
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

/// What each `job_read` call returned, oldest first.
fn reads(session: &nori_world::WorldSession) -> Vec<serde_json::Value> {
    session
        .recorder()
        .iter()
        .filter_map(|entry| match &entry.record {
            Record::HostTool { name, output, .. } if name == "job_read" => {
                serde_json::from_str(output).ok()
            }
            _ => None,
        })
        .collect()
}

fn delivered(events: &Events) -> usize {
    events
        .nori()
        .filter(|event| matches!(event, NoriEvent::Monitor(MonitorWire::Delivered(_))))
        .count()
}

/// The job is ready, then exits 3: the agent is woken for each, and reads each line once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_job_wakes_the_agent_when_ready_and_when_it_exits() {
    let session = boot(WORLD, "lead").await;
    session.ask("run the job").await.expect("the starting turn");
    session
        .wait_for("the ready wake", |_| {
            wakes(&session)
                .iter()
                .any(|wake| wake.contains("job ready: READY"))
        })
        .await
        .expect("the ready line is delivered");
    session
        .wait_for("the exit wake", |_| {
            wakes(&session).iter().any(|wake| wake.contains("exited 3"))
        })
        .await
        .expect("the exit is delivered");
    session.settle().await.expect("the wake turns end");
    let wakes = wakes(&session);
    let ready = wakes
        .iter()
        .position(|wake| wake.contains("job ready: READY"));
    let exit = wakes.iter().position(|wake| wake.contains("exited 3"));
    assert!(ready <= exit, "ready comes before the exit: {wakes:#?}");
    assert!(
        wakes.iter().all(|wake| wake.contains("source=\"job\"")),
        "{wakes:#?}"
    );

    session
        .ask("read the job output")
        .await
        .expect("the reading turn");
    let reads = reads(&session);
    assert_eq!(reads.len(), 2, "{reads:#?}");
    assert_eq!(reads[0]["stdout"], "booting\nREADY", "{reads:#?}");
    assert_eq!(reads[0]["state"], "exited 3", "{reads:#?}");
    assert_eq!(
        reads[1].get("stdout"),
        None,
        "nothing is read twice: {reads:#?}"
    );
    assert_eq!(reads[1]["lines"], 0, "{reads:#?}");
    assert!(delivered(&session.events()) >= 1);
    // The job's files are on this computer, as for any job: leave nothing behind.
    if let (Some(id), Some(home)) = (reads[0]["job"].as_str(), std::env::var_os("HOME")) {
        let _ = std::fs::remove_dir_all(std::path::Path::new(&home).join(".nori/jobs").join(id));
    }
}
