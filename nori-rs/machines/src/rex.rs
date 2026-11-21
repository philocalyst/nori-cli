//! The user's Rex hosts, read with the `rex` CLI.
//!
//! Rex keeps the list of the user's machines (`rex hosts`). A host with an `ssh://` endpoint is
//! reached at that endpoint. A host Rex reaches over HTTPS (its built-in Tailscale) is reached
//! over ssh by its label, which works when the user's ssh config has that alias; a probe finds
//! out. A host whose HTTPS endpoint is this computer's own Rex server is this computer.

use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;

use crate::MachineName;
use crate::SshDestination;

/// One Rex host as Nori uses it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RexHost {
    pub(crate) name: MachineName,
    pub(crate) route: RexRoute,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RexRoute {
    /// Rex stores an ssh endpoint.
    Ssh(SshDestination),
    /// Rex reaches it over HTTPS; ssh may know the label as an alias.
    Alias(SshDestination),
    /// It is this computer.
    ThisComputer,
}

#[derive(Deserialize)]
struct HostsReply {
    #[serde(default)]
    hosts: Vec<HostEntry>,
}

#[derive(Deserialize)]
struct HostEntry {
    host: Host,
}

#[derive(Deserialize)]
struct Host {
    #[serde(default)]
    label: String,
    #[serde(default)]
    endpoints: Vec<String>,
}

#[derive(Deserialize)]
struct StatusReply {
    #[serde(default)]
    tailscale: Option<Tailscale>,
}

#[derive(Deserialize)]
struct Tailscale {
    #[serde(default)]
    dns_name: Option<String>,
}

/// The `rex` CLI, when it is installed.
fn rex_binary() -> Option<PathBuf> {
    let on_path = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join("rex"))
            .find(|candidate| candidate.is_file())
    });
    on_path.or_else(|| {
        let dir = std::env::var_os("REX_BIN_DIR")?;
        let candidate = PathBuf::from(dir).join("rex");
        candidate.is_file().then_some(candidate)
    })
}

/// Run `rex ARGS` and return its stdout, giving up after `limit`.
fn rex_output(rex: &PathBuf, args: &[&str], limit: Duration) -> Option<String> {
    let mut child = Command::new(rex)
        .args(args)
        .args(["--timeout", "2s", "--autostart=false"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut stdout = stdout;
        let mut text = String::new();
        let _ = std::io::Read::read_to_string(&mut stdout, &mut text);
        let _ = tx.send(text);
    });
    match rx.recv_timeout(limit) {
        Ok(text) => {
            let _ = child.wait();
            Some(text)
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            None
        }
    }
}

/// The user's Rex hosts. Empty when Rex is not installed or does not answer.
pub(crate) fn hosts() -> Vec<RexHost> {
    let Some(rex) = rex_binary() else {
        return Vec::new();
    };
    let limit = Duration::from_secs(3);
    let Some(reply) = rex_output(&rex, &["hosts", "--json"], limit) else {
        return Vec::new();
    };
    let this_computer = rex_output(&rex, &["api", "call", "server.status", "{}"], limit)
        .and_then(|text| serde_json::from_str::<StatusReply>(&text).ok())
        .and_then(|status| status.tailscale?.dns_name);
    parse_hosts(&reply, this_computer.as_deref())
}

pub(crate) fn parse_hosts(reply: &str, this_computer: Option<&str>) -> Vec<RexHost> {
    let Ok(reply) = serde_json::from_str::<HostsReply>(reply) else {
        return Vec::new();
    };
    reply
        .hosts
        .into_iter()
        .filter_map(|entry| {
            let name: MachineName = entry.host.label.parse().ok()?;
            let ssh = entry
                .host
                .endpoints
                .iter()
                .find_map(|endpoint| SshDestination::from_ssh_url(endpoint));
            let is_this_computer = this_computer.is_some_and(|dns| {
                entry.host.endpoints.iter().any(|endpoint| {
                    endpoint
                        .strip_prefix("https://")
                        .is_some_and(|host| host.trim_end_matches('/') == dns)
                })
            });
            let route = match (ssh, is_this_computer) {
                (_, true) => RexRoute::ThisComputer,
                (Some(destination), false) => RexRoute::Ssh(destination),
                (None, false) => RexRoute::Alias(name.as_str().parse().ok()?),
            };
            Some(RexHost { name, route })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn rex_hosts_become_routes() {
        let reply = r#"{"home":{"label":""},"hosts":[
            {"host":{"label":"h16003mac","endpoints":["ssh://rmccrar6@h16003mac.tail7f9012.ts.net"]},"from":"home"},
            {"host":{"label":"mothership","endpoints":["https://rex-mothership.tailb43c34.ts.net"]},"from":"home"},
            {"host":{"label":"mac","endpoints":["https://rex-mac.tailb43c34.ts.net"]},"from":"home"},
            {"host":{"label":"bad label","endpoints":[]},"from":"home"}
        ]}"#;
        let name = |name: &str| name.parse::<MachineName>().ok();
        let destination = |text: &str| text.parse::<SshDestination>().ok();
        let hosts: Vec<_> = parse_hosts(reply, Some("rex-mac.tailb43c34.ts.net"))
            .into_iter()
            .map(|host| (Some(host.name), host.route))
            .collect();
        assert_eq!(
            hosts,
            vec![
                (
                    name("h16003mac"),
                    RexRoute::Ssh(
                        destination("rmccrar6@h16003mac.tail7f9012.ts.net")
                            .unwrap_or_else(|| unreachable!("a valid destination"))
                    )
                ),
                (
                    name("mothership"),
                    RexRoute::Alias(
                        destination("mothership").unwrap_or_else(|| unreachable!("valid"))
                    )
                ),
                (name("mac"), RexRoute::ThisComputer),
            ]
        );
    }
}
