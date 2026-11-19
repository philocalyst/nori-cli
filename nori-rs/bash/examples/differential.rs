//! Differential harness: runs the same scripts through `nori-bash` (brush, in-process) and a
//! reference GNU bash, and compares exit status, stdout and stderr.
//!
//! ```text
//! cargo run -p nori-bash --example differential            # markdown table, exit 1 on a
//!                                                          # divergence that can fake success
//! DIFF_BASH=/path/to/bash cargo run -p nori-bash --example differential -- -v
//! ```
//!
//! The reference is `$DIFF_BASH`, else Homebrew's bash, else any GNU bash 5 in the Nix store,
//! else `bash` on `PATH` (which must be GNU bash: macOS's `/bin/bash` is 3.2). A divergence is
//! *dangerous* when Nori reports success where bash fails, or prints output bash does not (such
//! as a trailing `PASS` marker). Other differences are listed but do not fail the run.

#![expect(
    clippy::expect_used,
    reason = "a dev harness: a fixture failure should fail loudly"
)]

use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;

use nori_bash::BashError;
use nori_bash::BashSession;
use nori_bash::ExecOptions;

/// One script, and how to run it.
struct Case {
    name: &'static str,
    script: &'static str,
    /// Milliseconds before the run is stopped; `None` for the default 20 seconds.
    timeout_ms: Option<i64>,
}

const fn case(name: &'static str, script: &'static str) -> Case {
    Case {
        name,
        script,
        timeout_ms: None,
    }
}

fn cases() -> Vec<Case> {
    vec![
        // errexit
        case(
            "e: function",
            "set -e; f(){ false; echo after; }; f; echo end",
        ),
        case(
            "e: function in ||",
            "set -e; f(){ false; echo after; }; f || echo handled; echo end",
        ),
        case("e: subshell", "set -e; (false; echo sub); echo end"),
        case("e: subshell exit status", "set -e; (exit 2); echo end"),
        case("e: $(...)", "set -e; x=$(false; echo hi); echo \"x=$x\""),
        case(
            "e: inherit_errexit $(...)",
            "set -e; shopt -s inherit_errexit; x=$(false; echo hi); echo \"x=$x\"",
        ),
        case("e: assignment status", "set -e; x=$(false); echo end"),
        case("e: false && true", "set -e; false && true; echo end"),
        case("e: true && false", "set -e; true && false; echo end"),
        case("e: false || false", "set -e; false || false; echo end"),
        case("e: pipeline last ok", "set -e; false | true; echo end"),
        case("e: pipeline last fails", "set -e; true | false; echo end"),
        case("e: pipefail", "set -eo pipefail; false | true; echo end"),
        case("e: !", "set -e; ! false; echo a; ! true; echo b"),
        case(
            "e: if/while conditions",
            "set -e; if false; then :; fi; while false; do :; done; echo end",
        ),
        case("e: until + [[ ]]", "set -e; [[ 1 -eq 2 ]]; echo end"),
        case("e: ((expr))", "set -e; (( 0 )); echo end"),
        case("e: group { }", "set -e; { false; echo grp; }; echo end"),
        case(
            "e: for body",
            "set -e; for i in 1 2; do false; echo $i; done; echo end",
        ),
        case(
            "e: command group in &&",
            "set -e; { false; echo grp; } && echo yes; echo end",
        ),
        case(
            "e: set +e then -e",
            "set -e; set +e; false; echo mid; set -e; false; echo end",
        ),
        // pipefail, PIPESTATUS
        case("pipefail status", "set -o pipefail; false | true; echo $?"),
        case("pipefail off", "false | true; echo $?"),
        case(
            "PIPESTATUS",
            "false | true | false; echo \"${PIPESTATUS[*]} $?\"",
        ),
        case(
            "pipefail rightmost failure",
            "set -o pipefail; (exit 3) | (exit 4) | true; echo $?",
        ),
        // traps
        case("trap ERR", "trap 'echo ERR $?' ERR; false; echo end"),
        case(
            "trap ERR + set -e",
            "set -e; trap 'echo ERR $?' ERR; false; echo end",
        ),
        case(
            "trap ERR errtrace fn",
            "set -E; trap 'echo ERR' ERR; f(){ false; }; f; echo end",
        ),
        case("trap EXIT", "trap 'echo EXIT' EXIT; echo body; exit 3"),
        case("trap EXIT status", "trap 'echo EXIT $?' EXIT; false"),
        case(
            "trap EXIT + set -e",
            "set -e; trap 'echo EXIT' EXIT; false; echo end",
        ),
        case("trap EXIT overrides status", "trap 'exit 0' EXIT; false"),
        // exit / return
        case("exit in subshell", "(exit 3); echo $?"),
        case("exit in function", "f(){ exit 4; }; f; echo notreached"),
        case("return code", "f(){ return 5; }; f; echo $?"),
        case("exit in $(...)", "x=$(exit 3); echo \"$? [$x]\""),
        case("exit status of last", "true; false"),
        case("exit n > 255", "exit 300"),
        case("exit negative", "exit -1"),
        // python
        case(
            "py heredoc syntax error",
            "python3 - <<'EOF'\nx = f\"{\nEOF\necho after $?",
        ),
        case(
            "py -c f-string error",
            "python3 -c 'print(f\"{\")'; echo after $?",
        ),
        case(
            "py heredoc + set -e",
            "set -e; python3 - <<'EOF'\nx = f\"{\nEOF\necho after",
        ),
        case(
            "py failing assert",
            "python3 -c 'assert False'; echo after $?",
        ),
        // not found
        case("not found 127", "nosuchcmd_xyz; echo $?"),
        case("not executable 126", "touch nx.sh; ./nx.sh; echo $?"),
        case("directory 126", "/etc; echo $?"),
        case("not found + set -e", "set -e; nosuchcmd_xyz; echo end"),
        case("not found in &&", "nosuchcmd_xyz && echo ok; echo $?"),
        case(
            "not found pipeline",
            "nosuchcmd_xyz | cat; echo ${PIPESTATUS[*]}",
        ),
        // signals
        case(
            "child killed TERM",
            "sleep 5 & p=$!; kill -TERM $p; wait $p; echo $?",
        ),
        case("child killed KILL", "sh -c 'kill -KILL $$'; echo $?"),
        case("subshell killed ($BASHPID)", "(kill -9 $BASHPID); echo $?"),
        case(
            "kill $$ (the shell itself)",
            "kill -TERM $$; echo notreached",
        ),
        case(
            "set -e child killed",
            "set -e; sh -c 'kill -TERM $$'; echo end",
        ),
        case(
            "bg pid is set",
            "sleep 0.2 & [ -n \"$!\" ] && echo set || echo empty; wait",
        ),
        case("wait $! status", "(exit 7) & wait $!; echo $?"),
        case("wait all status", "(exit 7) & wait; echo $?"),
        case("kill background job", "sleep 5 & kill $!; wait $!; echo $?"),
        case("kill background %1", "sleep 5 & kill %1; wait %1; echo $?"),
        case("last command killed TERM", "sh -c 'kill -TERM $$'"),
        case("last command killed KILL", "sh -c 'kill -KILL $$'"),
        case(
            "pipeline member killed",
            "set -o pipefail; sh -c 'kill -KILL $$' | cat; echo $?",
        ),
        case("kill 0 (process group)", "kill -TERM 0; echo notreached"),
        // PASS markers
        case(
            "PASS after failed test, no set -e",
            "python3 -c 'import sys; sys.exit(1)'; echo FINAL_PASS",
        ),
        case("PASS with ;", "false; echo FINAL_PASS"),
        case("PASS with &&", "false && echo FINAL_PASS"),
        case("PASS strict", "set -euo pipefail; false; echo FINAL_PASS"),
        case(
            "PASS strict pipe",
            "set -euo pipefail; python3 -c 'import sys; sys.exit(1)' | cat; echo FINAL_PASS",
        ),
        case(
            "PASS strict heredoc syntax",
            "set -euo pipefail; python3 - <<'EOF'\nx = f\"{\nEOF\necho FINAL_PASS",
        ),
        case("PASS strict ok", "set -euo pipefail; true; echo FINAL_PASS"),
        case(
            "PASS strict unset var",
            "set -euo pipefail; echo $undefined_var; echo FINAL_PASS",
        ),
        case(
            "PASS strict nested fn",
            "set -euo pipefail; check(){ python3 -c 'import sys; sys.exit(2)'; echo in_fn; }; check; echo FINAL_PASS",
        ),
        case(
            "PASS strict if-wrapped fn",
            "set -euo pipefail; check(){ false; echo in_fn; }; if check; then echo then; fi; echo FINAL_PASS",
        ),
        case(
            "PASS strict cmd subst arg",
            "set -euo pipefail; echo \"v=$(false)\"; echo FINAL_PASS",
        ),
        case(
            "PASS strict && chain",
            "set -euo pipefail; true && false && true; echo FINAL_PASS",
        ),
        case(
            "PASS strict local assign",
            "set -euo pipefail; f(){ local v=$(false); echo in_fn; }; f; echo FINAL_PASS",
        ),
        case(
            "PASS strict for/read",
            "set -euo pipefail; while read -r l; do false; done <<< 'a'; echo FINAL_PASS",
        ),
        case(
            "PASS strict arithmetic",
            "set -euo pipefail; (( 1 - 1 )); echo FINAL_PASS",
        ),
        // misc
        case("command not found in subshell", "(nosuchcmd_xyz); echo $?"),
        case("cd failure + set -e", "set -e; cd /no/such/dir; echo end"),
        case("source missing", "source ./missing.sh; echo $?"),
        case("redirect failure", "echo x > /no/such/dir/f; echo $?"),
        case(
            "redirect failure + set -e",
            "set -e; echo x > /no/such/dir/f; echo end",
        ),
        case("read from closed", "read x < /no/such; echo $?"),
        case("unset var -u", "set -u; echo $nope; echo end"),
        case("bad substitution", "echo ${!}x; echo $?"),
        case("syntax error", "echo ok; if then fi; echo $?"),
        Case {
            name: "timeout",
            script: "sleep 5; echo done",
            timeout_ms: Some(400),
        },
    ]
}

struct Run {
    /// `exit N`, or `error: ...` for a harness-level failure such as a timeout.
    status: String,
    stdout: String,
    stderr: String,
}

/// Whether `bash` runs here and is GNU bash 5 or later.
fn is_gnu_bash_5(bash: &str) -> bool {
    Command::new(bash)
        .arg("--version")
        .output()
        .ok()
        .is_some_and(|out| {
            let text = String::from_utf8_lossy(&out.stdout);
            text.starts_with("GNU bash, version 5") || text.starts_with("GNU bash, version 6")
        })
}

fn find_reference() -> String {
    if let Ok(path) = std::env::var("DIFF_BASH") {
        return path;
    }
    let mut candidates = vec![
        "/opt/homebrew/bin/bash".to_owned(),
        "/usr/local/bin/bash".to_owned(),
    ];
    if let Ok(entries) = std::fs::read_dir("/nix/store") {
        let mut nix: Vec<String> = entries
            .flatten()
            .filter(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                name.contains("-bash-") && !name.ends_with(".drv")
            })
            .map(|entry| entry.path().join("bin/bash").display().to_string())
            .collect();
        nix.sort();
        candidates.extend(nix);
    }
    candidates.push("bash".to_owned());
    candidates
        .into_iter()
        .find(|path| is_gnu_bash_5(path))
        .unwrap_or_else(|| "bash".to_owned())
}

fn run_reference(bash: &str, script: &str, cwd: &Path, timeout_ms: Option<i64>) -> Run {
    let mut child = match Command::new(bash)
        .process_group(0)
        .arg("-c")
        .arg(script)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return Run {
                status: format!("error: {error}"),
                stdout: String::new(),
                stderr: String::new(),
            };
        }
    };
    let limit =
        Duration::from_millis(u64::try_from(timeout_ms.unwrap_or(20_000)).unwrap_or(20_000));
    let started = std::time::Instant::now();
    let mut timed_out = false;
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => break,
            Ok(None) if started.elapsed() > limit => {
                timed_out = true;
                let _ = child.kill();
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    let output = child.wait_with_output();
    match output {
        Ok(output) => {
            use std::os::unix::process::ExitStatusExt as _;
            let status = if timed_out {
                format!("timeout after {}ms", limit.as_millis())
            } else if let Some(signal) = output.status.signal() {
                format!("signal {signal}")
            } else {
                format!("exit {}", output.status.code().unwrap_or(-1))
            };
            Run {
                status,
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            }
        }
        Err(error) => Run {
            status: format!("error: {error}"),
            stdout: String::new(),
            stderr: String::new(),
        },
    }
}

async fn run_nori(script: &str, cwd: &Path, timeout_ms: Option<i64>) -> Run {
    let mut session = match BashSession::new(cwd).await {
        Ok(session) => session,
        Err(error) => {
            return Run {
                status: format!("error: {error}"),
                stdout: String::new(),
                stderr: String::new(),
            };
        }
    };
    let result = session
        .exec_with_options(
            script,
            ExecOptions {
                timeout_ms: Some(timeout_ms.unwrap_or(20_000)),
                cancel: None,
            },
        )
        .await;
    match result {
        Ok(output) => Run {
            status: format!("exit {}", output.exit_code),
            stdout: output.stdout,
            stderr: output.stderr,
        },
        Err(BashError::Timeout {
            timeout_ms,
            stdout,
            stderr,
        }) => Run {
            status: format!("timeout after {timeout_ms}ms"),
            stdout,
            stderr,
        },
        Err(BashError::Cancelled { stdout, stderr }) => Run {
            status: "cancelled".to_owned(),
            stdout,
            stderr,
        },
        Err(error) => Run {
            status: format!("error: {error}"),
            stdout: String::new(),
            stderr: String::new(),
        },
    }
}

/// Run the script through nori-bash in a child process, so a script that kills "its own" process
/// (`kill $$`) is reported instead of taking the harness down with it.
fn run_nori_isolated(script: &str, cwd: &Path, timeout_ms: Option<i64>, linger_ms: i64) -> Run {
    let exe = std::env::current_exe().expect("current exe");
    let output = Command::new(exe)
        .process_group(0)
        .arg("--nori-child")
        .env("DIFF_SCRIPT", script)
        .env("DIFF_CWD", cwd)
        .env("DIFF_TIMEOUT_MS", timeout_ms.unwrap_or(20_000).to_string())
        .env("DIFF_LINGER_MS", linger_ms.to_string())
        .stdin(Stdio::null())
        .output();
    let output = match output {
        Ok(output) => output,
        Err(error) => {
            return Run {
                status: format!("error: {error}"),
                stdout: String::new(),
                stderr: String::new(),
            };
        }
    };
    {
        use std::os::unix::process::ExitStatusExt as _;
        if let Some(signal) = output.status.signal() {
            return Run {
                status: format!("NORI ITSELF killed by signal {signal}"),
                stdout: String::new(),
                stderr: String::new(),
            };
        }
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = text
        .lines()
        .rev()
        .find_map(|line| serde_json::from_str(line).ok())
        .unwrap_or(serde_json::Value::Null);
    let field = |name: &str| value[name].as_str().unwrap_or_default().to_owned();
    Run {
        status: field("status"),
        stdout: field("stdout"),
        stderr: field("stderr"),
    }
}

/// `signal N` and `exit 128+N` are the same death to a caller that only sees a status.
fn canonical(status: &str) -> String {
    match status
        .strip_prefix("signal ")
        .and_then(|n| n.parse::<i64>().ok())
    {
        Some(n) => format!("exit {}", 128 + n),
        None => status.to_owned(),
    }
}

fn failed(status: &str) -> bool {
    status != "exit 0"
}

/// Nori hid a failure: it reports success where bash fails, or prints output bash does not.
fn dangerous(reference: &Run, nori: &Run) -> bool {
    (failed(&reference.status) && !failed(&nori.status))
        || (nori.stdout != reference.stdout
            && nori.stdout.contains("PASS")
            && !reference.stdout.contains("PASS"))
}

/// Normalise what differs by design: the shell's name and line numbers in diagnostics.
fn normalise(stderr: &str) -> String {
    stderr
        .lines()
        .map(|line| {
            let line = line.trim();
            let line = line.strip_prefix("bash: ").unwrap_or(line);
            match line.strip_prefix("line ") {
                Some(rest) => rest
                    .split_once(": ")
                    .map_or(rest, |(_, text)| text)
                    .to_owned(),
                None => line.to_owned(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::main]
async fn main() {
    if std::env::args().any(|arg| arg == "--nori-child") {
        let script = std::env::var("DIFF_SCRIPT").unwrap_or_default();
        let cwd = std::env::var("DIFF_CWD").unwrap_or_default();
        let timeout = std::env::var("DIFF_TIMEOUT_MS")
            .ok()
            .and_then(|ms| ms.parse().ok());
        let run = run_nori(&script, Path::new(&cwd), timeout).await;
        println!(
            "{}",
            serde_json::json!({"status": run.status, "stdout": run.stdout, "stderr": run.stderr})
        );
        // A host outlives the call: let background work show whether the call stopped it.
        if let Some(linger) = std::env::var("DIFF_LINGER_MS")
            .ok()
            .and_then(|ms| ms.parse().ok())
        {
            tokio::time::sleep(Duration::from_millis(linger)).await;
        }
        return;
    }
    let verbose = std::env::args().any(|arg| arg == "-v");
    let bash = find_reference();
    let version = Command::new(&bash)
        .arg("--version")
        .output()
        .map(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .to_owned()
        })
        .unwrap_or_default();
    println!("reference: {bash} ({version})");
    println!("| case | bash | nori | stdout | stderr | verdict |");
    println!("| --- | --- | --- | --- | --- | --- |");
    let mut dangerous_count = 0;
    let mut different = 0;
    for case in cases() {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().canonicalize().expect("canonical");
        let reference = run_reference(&bash, case.script, &cwd, case.timeout_ms);
        let temp_nori = tempfile::tempdir().expect("tempdir");
        let cwd_nori = temp_nori.path().canonicalize().expect("canonical");
        let nori = run_nori_isolated(case.script, &cwd_nori, case.timeout_ms, 0);
        let stdout_same = reference.stdout == nori.stdout;
        let stderr_same = normalise(&reference.stderr) == normalise(&nori.stderr);
        let status_same = canonical(&reference.status) == canonical(&nori.status);
        let verdict = if dangerous(&reference, &nori) {
            dangerous_count += 1;
            "**DANGEROUS**"
        } else if !status_same || !stdout_same {
            different += 1;
            "differs"
        } else {
            "same"
        };
        println!(
            "| {} | {} | {} | {} | {} | {} |",
            case.name,
            reference.status,
            nori.status,
            if stdout_same { "=" } else { "≠" },
            if stderr_same { "=" } else { "≠" },
            verdict
        );
        if verbose && verdict != "same" || (verbose && !stderr_same) {
            println!("\n<details><summary>{}</summary>\n", case.name);
            println!("script: `{}`\n", case.script.replace('\n', "\\n"));
            println!(
                "bash   stdout={:?} stderr={:?}",
                reference.stdout, reference.stderr
            );
            println!("nori   stdout={:?} stderr={:?}\n", nori.stdout, nori.stderr);
            println!("</details>\n");
        }
    }
    println!("\nProbes (nori only): what a timeout leaves behind");
    for (name, script) in [
        ("timeout stops a foreground child", "sleep 2; touch marker"),
        (
            "timeout stops a background job",
            "(sleep 2; touch marker) & wait",
        ),
    ] {
        let temp = tempfile::tempdir().expect("tempdir");
        let cwd = temp.path().canonicalize().expect("canonical");
        let run = run_nori_isolated(script, &cwd, Some(300), 3500);
        let leaked = cwd.join("marker").exists();
        println!(
            "- {name}: status `{}`, the work {} after the timeout",
            run.status,
            if leaked {
                "**kept running**"
            } else {
                "was stopped"
            }
        );
    }
    println!(
        "\n{dangerous_count} dangerous, {different} differing, of {} cases",
        cases().len()
    );
    if dangerous_count > 0 {
        std::process::exit(1);
    }
}
