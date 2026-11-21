//! Run a command on one of your machines the way Nori's `bash` tool does.
//!
//! `cargo run -p nori-machines --example machine_run -- MACHINE 'COMMAND' [DIR]`, with `NORI_HOME`
//! pointing at a Nori home (its `config.toml` `[machines]` and the facts cache are used).

use std::path::PathBuf;
use std::time::Duration;

use nori_machines::RemoteDir;
use nori_machines::Roster;
use nori_machines::StartDir;
use nori_machines::Target;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let home = std::env::var_os("NORI_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let roster = Roster::discover(&home);
    for line in roster.describe() {
        println!("{line}");
    }
    let (Some(name), Some(command)) = (args.first(), args.get(1)) else {
        eprintln!("usage: machine_run MACHINE COMMAND [DIR]");
        return;
    };
    let machine = match roster.resolve(Some(name)) {
        Ok(Target::Remote(machine)) => machine.clone(),
        Ok(Target::Local) => {
            eprintln!("{name} is this computer");
            return;
        }
        Err(error) => {
            eprintln!("{error}");
            return;
        }
    };
    let start = match args.get(2) {
        Some(dir) => StartDir::Required(RemoteDir::parse(dir)),
        None => StartDir::FirstOf(
            roster.default_dirs(&machine, &std::env::current_dir().unwrap_or_default()),
        ),
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel::<nori_machines::Line>(64);
    let printer = tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            println!("  [{:?}] {}", line.stream, line.text);
        }
    });
    let run = roster
        .exec(
            &machine,
            command,
            &start,
            Some(Duration::from_secs(30)),
            Some(tx),
        )
        .await;
    let _ = printer.await;
    match run {
        Ok(run) => println!(
            "exit {} in {}ms; started in {}, ended in {}",
            run.exit_code, run.duration_ms, run.started_in, run.cwd
        ),
        Err(error) => println!("error: {error}"),
    }
    for line in roster.describe() {
        println!("{line}");
    }
}
