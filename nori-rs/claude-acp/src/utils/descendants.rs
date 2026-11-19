//! Reaping the processes a session leaves behind.
//!
//! Killing the Claude CLI orphans whatever its tools started (a running shell
//! command, a dev server), and those carry on after the adapter is gone. The
//! tree is read before the CLI is stopped, because afterwards the orphans are
//! re-parented and can no longer be traced back to us.

use std::collections::HashMap;
use std::time::Duration;

/// The Claude CLI this process started for `session_id`, and everything under it, descendants
/// first. The CLI is found by the session id on its command line (`--session-id` or
/// `--resume`); empty when there is none.
#[cfg(unix)]
pub fn session_tree(session_id: &str) -> Vec<i32> {
    let table = process_table();
    let us = i32::try_from(std::process::id()).unwrap_or(i32::MAX);
    let Some(cli) = table
        .iter()
        .find(|process| process.ppid == us && process.args.contains(session_id))
        .map(|process| process.pid)
    else {
        return Vec::new();
    };
    let mut found = tree(&table, cli);
    found.push(cli);
    found
}

#[cfg(unix)]
struct Process {
    pid: i32,
    ppid: i32,
    args: String,
}

/// Every running process, from `ps`.
#[cfg(unix)]
fn process_table() -> Vec<Process> {
    let Ok(output) = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,args="])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.trim_start().splitn(3, char::is_whitespace);
            let pid = fields.next()?.parse().ok()?;
            let ppid = fields.next()?.trim().parse().ok()?;
            let args = fields.next().unwrap_or_default().trim().to_owned();
            Some(Process { pid, ppid, args })
        })
        .collect()
}

/// The descendants of `root` in `table`, children before their parents' siblings.
#[cfg(unix)]
fn tree(table: &[Process], root: i32) -> Vec<i32> {
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    for process in table {
        children.entry(process.ppid).or_default().push(process.pid);
    }
    let mut found = Vec::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for &child in children.get(&parent).into_iter().flatten() {
            found.push(child);
            frontier.push(child);
        }
    }
    found
}

/// Whether `pid` is still a running process.
#[cfg(unix)]
fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Ask `pids` to stop, then force whichever are still there after `grace`.
///
/// Returns as soon as they are all gone, so a clean shutdown costs nothing.
#[cfg(unix)]
pub async fn terminate(pids: &[i32], grace: Duration) {
    let send = |pids: &[i32], signal: i32| {
        for &pid in pids {
            // SAFETY: kill(2) only sends a signal; a stale pid yields ESRCH.
            unsafe { libc::kill(pid, signal) };
        }
    };
    let mut remaining: Vec<i32> = pids.iter().copied().filter(|&pid| alive(pid)).collect();
    if remaining.is_empty() {
        return;
    }
    send(&remaining, libc::SIGTERM);
    let deadline = tokio::time::Instant::now() + grace;
    while tokio::time::Instant::now() < deadline {
        remaining.retain(|&pid| alive(pid));
        if remaining.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    send(&remaining, libc::SIGKILL);
}

#[cfg(not(unix))]
pub fn descendants() -> Vec<i32> {
    Vec::new()
}

#[cfg(not(unix))]
pub fn session_tree(_session_id: &str) -> Vec<i32> {
    Vec::new()
}

#[cfg(not(unix))]
pub async fn terminate(_pids: &[i32], _grace: Duration) {}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_session_tree_is_its_cli_and_what_the_cli_started() {
        let session_id = "4f1c0f7e-0000-4000-8000-tree-test";
        // A stand-in CLI that starts a tool process of its own.
        let mut cli = std::process::Command::new("sh")
            .args(["-c", "sleep 30 & wait", "sh", "--session-id", session_id])
            .spawn()
            .expect("spawn sh");
        let cli_pid = i32::try_from(cli.id()).expect("pid fits");
        let mut found = Vec::new();
        for _ in 0..100 {
            found = session_tree(session_id);
            if found.len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found.last(), Some(&cli_pid));
        assert_eq!(session_tree("no-such-session"), Vec::<i32>::new());

        terminate(&found, Duration::from_millis(50)).await;
        assert!(!cli.wait().expect("wait").success());
    }
}
