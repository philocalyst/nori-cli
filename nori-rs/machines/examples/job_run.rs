//! Start a job on one of your machines, follow it, drop it, adopt it again, and stop it.
//!
//! `cargo run -p nori-machines --example job_run -- MACHINE JOBS_ROOT 'COMMAND'`, with `NORI_HOME`
//! pointing at a Nori home. JOBS_ROOT is an absolute directory on that machine for the job files.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use nori_machines::Roster;
use nori_machines::StartDir;
use nori_machines::jobs::Cursor;
use nori_machines::jobs::JobSpec;
use nori_machines::jobs::MachineSupervisor;
use nori_machines::jobs::Place;
use nori_machines::jobs::ReadFrom;
use nori_machines::jobs::Supervisor;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(machine), Some(root), Some(command)) = (args.first(), args.get(1), args.get(2))
    else {
        eprintln!("usage: job_run MACHINE JOBS_ROOT COMMAND");
        return;
    };
    let home = std::env::var_os("NORI_HOME").map_or_else(|| PathBuf::from("."), PathBuf::from);
    let roster = Arc::new(Roster::discover(&home));
    let supervisor = MachineSupervisor::with_root(Arc::clone(&roster), root);
    let place = match machine.as_str() {
        "local" => Place::Local,
        name => Place::Machine(name.parse().unwrap_or_else(|_| panic!("bad name"))),
    };
    let started = Instant::now();
    let job = match supervisor
        .start(JobSpec {
            command: command.clone(),
            place,
            start: StartDir::FirstOf(Vec::new()),
            env: BTreeMap::new(),
            label: "example".to_owned(),
        })
        .await
    {
        Ok(job) => job,
        Err(error) => {
            println!("start failed: {error}");
            return;
        }
    };
    println!(
        "started {} pid {} in {:?}",
        job.record().id,
        job.record().pid,
        started.elapsed()
    );
    let mut lines = job.lines();
    let first = tokio::time::timeout(Duration::from_secs(10), lines.recv()).await;
    println!("first line after {:?}: {first:?}", started.elapsed());
    let record = job.record().clone();
    drop(job);
    println!("dropped the handle; adopting again");
    let adopted = match supervisor.adopt(record).await {
        Ok(job) => job,
        Err(error) => {
            println!("adopt failed: {error}");
            return;
        }
    };
    println!("adopted: {:?}", *adopted.state().borrow());
    let all = adopted
        .read(ReadFrom::Cursor(Cursor::default()), 1 << 20)
        .await;
    println!(
        "read from start: {:?}",
        all.map(|output| (output.lines.len(), output.next))
    );
    adopted.stop(Duration::from_secs(1)).await.ok();
    println!(
        "after stop: {:?}",
        adopted.wait(Duration::from_secs(10)).await
    );
}
