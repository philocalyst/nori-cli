//! Load, validate and optionally dry-run a `nori:Program` module.
//!
//! ```text
//! cargo run -p nori-conduit --example program_check -- <program.pkl> [options]
//!   --run                      execute against the in-memory LocalHost (no agents are started)
//!   --journal <dir>            write <dir>/{journal.jsonl,state.pkl} and reload the snapshot (implies --run)
//!   --json                     print the compiled op graph as JSON, then exit
//!   --fail <alias>:<n>:<class> the first <n> sends to <alias> fail with failure class <class>
//!   --approve <alias>:<turn>   <alias> replies APPROVED from its <turn>th turn on
//!   --signal <topic>=<payload> send the run a signal as soon as it starts
//!   --arg <name>=<value>       an argument of a skill (`nori:Skill`)
//! ```

use futures::StreamExt;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;

use nori_conduit::ConduitError;
use nori_conduit::Envelope;
use nori_conduit::FileTransport;
use nori_conduit::LocalHost;
use nori_conduit::RecordingTransport;
use nori_conduit::RunConfig;
use nori_conduit::RunHandle;
use nori_conduit::RunSnapshot;
use nori_conduit::Transport;
use nori_conduit::WireOp;
use nori_workflow::FailureKind;

const USAGE: &str = "usage: program_check <program.pkl> [--run] [--journal <dir>] [--json] [--fail alias:n:class] [--approve alias:turn] [--arg name=value]";

/// The program (or skill, with its arguments) the options name.
fn load(options: &Options) -> Result<nori_conduit::CompiledProgram, String> {
    let source = std::fs::read_to_string(&options.path)
        .map_err(|error| format!("cannot read {}: {error}", options.path.display()))?;
    nori_conduit::load_source_with(
        &source,
        Some(&options.path),
        BTreeMap::new(),
        &options.arguments,
    )
    .map_err(|error| error.to_string())
}

#[derive(Default)]
struct Options {
    path: PathBuf,
    run: bool,
    json: bool,
    journal: Option<PathBuf>,
    failures: Vec<String>,
    approvals: Vec<String>,
    signals: Vec<String>,
    /// A skill's arguments, `--arg name=value`.
    arguments: BTreeMap<String, String>,
}

fn parse_options() -> Result<Options, String> {
    let mut args = std::env::args().skip(1);
    let mut options = Options {
        path: PathBuf::from(args.next().ok_or(USAGE)?),
        ..Options::default()
    };
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--run" => options.run = true,
            "--json" => options.json = true,
            "--journal" => {
                options.journal = Some(PathBuf::from(args.next().ok_or(USAGE)?));
                options.run = true;
            }
            "--fail" => options.failures.push(args.next().ok_or(USAGE)?),
            "--approve" => options.approvals.push(args.next().ok_or(USAGE)?),
            "--signal" => options.signals.push(args.next().ok_or(USAGE)?),
            "--arg" => {
                let argument = args.next().ok_or(USAGE)?;
                let (name, value) = argument
                    .split_once('=')
                    .ok_or_else(|| format!("--arg expects name=value, got {argument}"))?;
                options.arguments.insert(name.to_owned(), value.to_owned());
            }
            other => return Err(format!("unknown option {other}\n{USAGE}")),
        }
    }
    Ok(options)
}

/// How a simulated actor behaves: how many of its first sends fail, with what class, and the
/// turn from which it approves.
#[derive(Default)]
struct Script {
    failures: i64,
    class: Option<FailureKind>,
    approve_from: Option<i64>,
}

fn scripts(options: &Options) -> Result<BTreeMap<String, Script>, String> {
    let mut scripts: BTreeMap<String, Script> = BTreeMap::new();
    for spec in &options.failures {
        let parts: Vec<&str> = spec.split(':').collect();
        let [alias, count, class] = parts[..] else {
            return Err(format!("--fail expects alias:count:class, got {spec}"));
        };
        let script = scripts.entry(alias.to_owned()).or_default();
        script.failures = count
            .parse()
            .map_err(|_| format!("`{count}` is not a count"))?;
        script.class = Some(
            FailureKind::parse(class).ok_or_else(|| format!("`{class}` is not a failure class"))?,
        );
    }
    for spec in &options.approvals {
        let (alias, turn) = spec
            .split_once(':')
            .ok_or_else(|| format!("--approve expects alias:turn, got {spec}"))?;
        scripts.entry(alias.to_owned()).or_default().approve_from = Some(
            turn.parse()
                .map_err(|_| format!("`{turn}` is not a turn"))?,
        );
    }
    Ok(scripts)
}

fn reply(message: &str, turn: i64, approve_from: Option<i64>) -> String {
    if approve_from.is_some_and(|from| turn >= from) {
        format!("APPROVED (turn {turn})")
    } else {
        let excerpt: String = message.chars().take(50).collect();
        format!("acknowledged turn {turn}: {excerpt}")
    }
}

fn print_envelope(envelope: &Envelope) {
    if matches!(envelope.op, WireOp::RunStarted) {
        println!(
            "  {:>3} {:<12} {}",
            envelope.seq,
            envelope.actor,
            envelope.op.name()
        );
    } else {
        println!(
            "  {:>3} {:<12} t{:<2} {:<17} {}",
            envelope.seq,
            envelope.actor,
            envelope.turn,
            envelope.op.name(),
            envelope.payload
        );
    }
}

async fn dry_run(options: &Options) -> Result<(), String> {
    let program = load(options)?;
    let host = Arc::new(LocalHost::new(std::env::temp_dir()));
    for (alias, script) in scripts(options)? {
        let seen = AtomicI64::new(0);
        host.script_actor(alias, move |message, turn| {
            if let Some(class) = script.class
                && seen.fetch_add(1, Ordering::SeqCst) < script.failures
            {
                return Err(ConduitError::Host {
                    class,
                    message: "simulated failure".to_owned(),
                    retry_after_ms: None,
                });
            }
            Ok(reply(message, turn, script.approve_from))
        });
    }
    let transport: Arc<dyn Transport> = match &options.journal {
        Some(dir) => Arc::new(FileTransport::open(dir).map_err(|error| error.to_string())?),
        None => Arc::new(RecordingTransport::default()),
    };
    let mut config = RunConfig::new(host.clone(), transport);
    config.run_dir = options.journal.clone();
    config.source = Some(options.path.clone());

    // The handle is how another world sees the run: watch it live, feed it signals.
    let handle = RunHandle::start(program, config);
    let mut events = handle.subscribe();
    for spec in &options.signals {
        let (topic, payload) = spec.split_once('=').unwrap_or((spec, ""));
        handle
            .signal(topic, serde_json::Value::String(payload.to_owned()))
            .map_err(|error| error.to_string())?;
    }
    println!("\njournal:");
    let outcome = {
        let printer = tokio::spawn(async move {
            let mut count = 0;
            while let Some(envelope) = events.next().await {
                count += 1;
                print_envelope(&envelope);
                if matches!(envelope.op, WireOp::RunFinished | WireOp::RunFailed) {
                    break;
                }
            }
            count
        });
        let outcome = handle.wait().await;
        let count = printer.await.map_err(|error| error.to_string())?;
        println!("({count} envelopes)");
        outcome
    };
    println!("\nhost log:");
    for line in host.log() {
        println!("  {line}");
    }
    if let Some(dir) = &options.journal {
        let snapshot =
            RunSnapshot::load(&dir.join("state.pkl")).map_err(|error| error.to_string())?;
        println!(
            "\nstate.pkl reloaded: run {} is {:?} at seq {} with {} actor(s) and {} binding(s)",
            snapshot.run,
            snapshot.status,
            snapshot.seq,
            snapshot.actors.len(),
            snapshot.bindings.len()
        );
    }
    let outcome = outcome.map_err(|error| format!("run failed: {error}"))?;
    println!(
        "\nrun {} finished: {}",
        outcome.run,
        outcome.value.display_compact()
    );
    println!("bindings: {:?}", outcome.bindings);
    Ok(())
}

fn main() -> ExitCode {
    let options = match parse_options() {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(2);
        }
    };
    let program = match load(&options) {
        Ok(program) => program,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    if options.json {
        match serde_json::to_string_pretty(&program) {
            Ok(json) => println!("{json}"),
            Err(error) => {
                eprintln!("{error}");
                return ExitCode::FAILURE;
            }
        }
        return ExitCode::SUCCESS;
    }
    println!(
        "{}: valid program `{}`: {} actor(s), {} world(s), {} declared tool(s)",
        options.path.display(),
        program.name,
        program.actors.len(),
        program.worlds.len(),
        program.tools.len()
    );
    if !options.run {
        return ExitCode::SUCCESS;
    }
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("cannot start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(dry_run(&options)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}
