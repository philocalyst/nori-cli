//! The machines Nori may run commands on, where they come from, and what is known about them.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::PoisonError;
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::mpsc;

use crate::ExecError;
use crate::Facts;
use crate::Line;
use crate::MachineName;
use crate::RemoteDir;
use crate::Run;
use crate::SshDestination;
use crate::StartDir;
use crate::exec::Connection;
use crate::exec::Via;
use crate::facts::FactsCache;
use crate::name::LOCAL;
use crate::rex;
use crate::rex::RexRoute;
use crate::script::ExecScript;

/// One machine the agent may name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Machine {
    pub name: MachineName,
    pub ssh: SshDestination,
    /// What the user said it is for.
    pub description: Option<String>,
    /// Where commands start when nothing better is known.
    pub cwd: Option<RemoteDir>,
    /// Nori there, when it is not `nori` on the login `PATH`, and the Nori home it uses: what a
    /// session moved there runs.
    pub nori: NoriThere,
    /// Where jobs keep their files there, when not `~/.nori/jobs`.
    pub jobs_dir: Option<RemoteDir>,
    pub origin: Origin,
}

/// How to run Nori on a machine.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NoriThere {
    /// The program; `nori` when not configured.
    pub program: Option<String>,
    /// `NORI_HOME` for it; its own default when not configured.
    pub home: Option<String>,
}

/// Where Nori learned of a machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// A Rex host with an ssh endpoint.
    RexSsh,
    /// A Rex host reached by its label as an ssh alias.
    RexAlias,
    /// `[machines.NAME]` in Nori's config.
    Config,
}

/// Where a command runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target<'a> {
    /// This computer.
    Local,
    Remote(&'a Machine),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("there is no machine named `{requested}`; {known}")]
pub struct UnknownMachine {
    pub requested: String,
    known: String,
}

/// How the last probe of a machine went, this run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reach {
    Reachable(Facts),
    Unreachable { reason: String },
}

/// The user's machines.
#[derive(Debug)]
pub struct Roster {
    machines: BTreeMap<MachineName, Machine>,
    /// Rex labels that name this computer: the agent may use them for `local`.
    this_computer: Vec<MachineName>,
    known: Mutex<KnownFacts>,
    cache_path: Option<PathBuf>,
    control_dir: Option<PathBuf>,
}

#[derive(Debug, Default)]
struct KnownFacts {
    /// From earlier runs, until a probe this run replaces them.
    cached: BTreeMap<MachineName, Facts>,
    /// When each machine last failed to answer, and why, this run or an earlier one.
    failed: BTreeMap<MachineName, (i64, String)>,
    /// Probed this run.
    probed: BTreeMap<MachineName, Reach>,
}

#[derive(Deserialize, Default)]
struct ConfigFile {
    #[serde(default)]
    machines: Option<MachinesSection>,
}

#[derive(Deserialize)]
struct MachinesSection {
    /// Import the user's Rex hosts.
    #[serde(default = "import_rex_hosts")]
    rex_hosts: bool,
    #[serde(flatten)]
    entries: BTreeMap<String, MachineEntry>,
}

fn import_rex_hosts() -> bool {
    true
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MachineEntry {
    ssh: Option<String>,
    description: Option<String>,
    cwd: Option<String>,
    nori: Option<String>,
    nori_home: Option<String>,
    jobs_dir: Option<String>,
    #[serde(default)]
    hidden: bool,
}

impl Roster {
    /// No machines: commands run on this computer only.
    pub fn empty() -> Self {
        Self {
            machines: BTreeMap::new(),
            this_computer: Vec::new(),
            known: Mutex::new(KnownFacts::default()),
            cache_path: None,
            control_dir: None,
        }
    }

    /// Read the user's machines: Rex's hosts (unless `[machines] rex_hosts = false`), then
    /// `[machines.NAME]` in `config.toml`, which adds machines and describes, re-routes or hides
    /// Rex's. Blocks for the `rex` CLI, at most a few seconds.
    pub fn discover(nori_home: &Path) -> Self {
        let config = std::fs::read_to_string(nori_home.join("config.toml"))
            .ok()
            .and_then(|text| match toml::from_str::<ConfigFile>(&text) {
                Ok(config) => Some(config),
                Err(error) => {
                    tracing::warn!("ignoring [machines] in config.toml: {error}");
                    None
                }
            })
            .and_then(|config| config.machines);
        let rex_hosts = if config.as_ref().is_none_or(|section| section.rex_hosts) {
            rex::hosts()
        } else {
            Vec::new()
        };
        let mut roster = Self::from_sources(rex_hosts, config);
        roster.cache_path = Some(FactsCache::path(nori_home));
        let cache = FactsCache::load(&FactsCache::path(nori_home));
        roster.known = Mutex::new(KnownFacts {
            cached: cache.machines,
            failed: cache.unreachable,
            probed: BTreeMap::new(),
        });
        roster.control_dir = control_dir(nori_home);
        roster
    }

    fn from_sources(rex_hosts: Vec<rex::RexHost>, config: Option<MachinesSection>) -> Self {
        let mut machines = BTreeMap::new();
        let mut this_computer = Vec::new();
        for host in rex_hosts {
            let (ssh, origin) = match host.route {
                RexRoute::Ssh(destination) => (destination, Origin::RexSsh),
                RexRoute::Alias(destination) => (destination, Origin::RexAlias),
                RexRoute::ThisComputer => {
                    this_computer.push(host.name);
                    continue;
                }
            };
            machines.insert(
                host.name.clone(),
                Machine {
                    name: host.name,
                    ssh,
                    description: None,
                    cwd: None,
                    nori: NoriThere::default(),
                    jobs_dir: None,
                    origin,
                },
            );
        }
        for (name, entry) in config.map(|section| section.entries).unwrap_or_default() {
            let name: MachineName = match name.parse() {
                Ok(name) => name,
                Err(error) => {
                    tracing::warn!("ignoring [machines.{name}]: {error}");
                    continue;
                }
            };
            if entry.hidden {
                machines.remove(&name);
                continue;
            }
            let ssh = match entry.ssh.as_deref().map(str::parse::<SshDestination>) {
                Some(Ok(ssh)) => Some(ssh),
                Some(Err(error)) => {
                    tracing::warn!("ignoring [machines.{name}]: {error}");
                    continue;
                }
                None => None,
            };
            let existing = machines.remove(&name);
            let Some(ssh) = ssh.or_else(|| existing.as_ref().map(|machine| machine.ssh.clone()))
            else {
                tracing::warn!("ignoring [machines.{name}]: it needs `ssh = \"user@host\"`");
                continue;
            };
            let origin = match (&entry.ssh, &existing) {
                (None, Some(existing)) => existing.origin,
                (Some(_), _) | (None, None) => Origin::Config,
            };
            machines.insert(
                name.clone(),
                Machine {
                    name,
                    ssh,
                    description: entry
                        .description
                        .map(|text| text.trim().to_owned())
                        .filter(|text| !text.is_empty()),
                    cwd: entry.cwd.as_deref().map(RemoteDir::parse),
                    nori: NoriThere {
                        program: entry.nori,
                        home: entry.nori_home,
                    },
                    jobs_dir: entry.jobs_dir.as_deref().map(RemoteDir::parse),
                    origin,
                },
            );
        }
        this_computer.retain(|name| !machines.contains_key(name));
        Self {
            machines,
            this_computer,
            known: Mutex::new(KnownFacts::default()),
            cache_path: None,
            control_dir: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.machines.is_empty()
    }

    /// What the user's other machines call this computer (its Rex label), when they do.
    pub fn this_computer(&self) -> Option<&MachineName> {
        self.this_computer.first()
    }

    pub fn machines(&self) -> impl Iterator<Item = &Machine> {
        self.machines.values()
    }

    /// What `machine` means: nothing, `local` and a label of this computer are this computer.
    pub fn resolve(&self, requested: Option<&str>) -> Result<Target<'_>, UnknownMachine> {
        let Some(requested) = requested.map(str::trim).filter(|name| !name.is_empty()) else {
            return Ok(Target::Local);
        };
        if requested.eq_ignore_ascii_case(LOCAL)
            || self
                .this_computer
                .iter()
                .any(|name| name.as_str() == requested)
        {
            return Ok(Target::Local);
        }
        if let Some(machine) = requested
            .parse::<MachineName>()
            .ok()
            .and_then(|name| self.machines.get(&name))
        {
            return Ok(Target::Remote(machine));
        }
        let names: Vec<&str> = self.machines.keys().map(MachineName::as_str).collect();
        Err(UnknownMachine {
            requested: requested.to_owned(),
            known: if names.is_empty() {
                "commands run on this computer only (no machines are set up)".to_owned()
            } else {
                format!(
                    "the machines are `local` (this computer), {}",
                    names.join(", ")
                )
            },
        })
    }

    /// What is known of `name`: this run's probe, else the last run's.
    pub fn reach(&self, name: &MachineName) -> Option<Reach> {
        let known = self.known.lock().unwrap_or_else(PoisonError::into_inner);
        known
            .probed
            .get(name)
            .cloned()
            .or_else(|| known.cached.get(name).cloned().map(Reach::Reachable))
            .or_else(|| {
                known
                    .failed
                    .get(name)
                    .map(|(_, reason)| Reach::Unreachable {
                        reason: reason.clone(),
                    })
            })
    }

    fn facts_this_run(&self, name: &MachineName) -> Option<Facts> {
        let known = self.known.lock().unwrap_or_else(PoisonError::into_inner);
        match known.probed.get(name) {
            Some(Reach::Reachable(facts)) => Some(facts.clone()),
            Some(Reach::Unreachable { .. }) | None => None,
        }
    }

    pub(crate) fn connection<'a>(&'a self, machine: &'a Machine) -> Connection<'a> {
        Connection {
            machine: &machine.name,
            via: Via::Ssh {
                destination: &machine.ssh,
                control_dir: self.control_dir.as_deref(),
            },
        }
    }

    /// Connect to `machine` and learn what it is; remembered for this run and the next.
    pub async fn probe(&self, machine: &Machine) -> Result<Facts, ExecError> {
        let outcome = self.connection(machine).probe().await.and_then(|stdout| {
            Facts::parse(&stdout, crate::facts::now_seconds()).ok_or_else(|| {
                ExecError::Unreachable {
                    machine: machine.name.clone(),
                    destination: machine.ssh.clone(),
                    reason: "it answered, but not as a POSIX shell".to_owned(),
                }
            })
        });
        let mut known = self.known.lock().unwrap_or_else(PoisonError::into_inner);
        match &outcome {
            Ok(facts) => {
                known
                    .probed
                    .insert(machine.name.clone(), Reach::Reachable(facts.clone()));
                known.cached.insert(machine.name.clone(), facts.clone());
                known.failed.remove(&machine.name);
            }
            Err(error) => {
                known.probed.insert(
                    machine.name.clone(),
                    Reach::Unreachable {
                        reason: error.to_string(),
                    },
                );
                known.failed.insert(
                    machine.name.clone(),
                    (crate::facts::now_seconds(), error.to_string()),
                );
            }
        }
        if let Some(path) = &self.cache_path {
            let cache = FactsCache {
                machines: known.cached.clone(),
                unreachable: known.failed.clone(),
            };
            if let Err(error) = cache.save(path) {
                tracing::warn!("could not save machine facts: {error}");
            }
        }
        outcome
    }

    /// Probe at once every machine not probed within `max_age`: what the agent is told about
    /// machines stays current without connecting to each one at every start.
    pub async fn probe_stale(self: Arc<Self>, max_age: Duration) {
        let now = crate::facts::now_seconds();
        let stale: Vec<Machine> = {
            let known = self.known.lock().unwrap_or_else(PoisonError::into_inner);
            self.machines
                .values()
                .filter(|machine| {
                    known
                        .cached
                        .get(&machine.name)
                        .is_none_or(|facts| now - facts.probed_at > max_age.as_secs() as i64)
                        && known
                            .failed
                            .get(&machine.name)
                            .is_none_or(|(at, _)| now - at > RETRY_UNREACHABLE_SECONDS)
                })
                .cloned()
                .collect()
        };
        let mut probes = tokio::task::JoinSet::new();
        for machine in stale {
            let roster = Arc::clone(&self);
            probes.spawn(async move {
                // Remembered either way; nobody is waiting on the answer.
                let _ = roster.probe(&machine).await;
            });
        }
        while probes.join_next().await.is_some() {}
    }

    /// What this run knows of `machine`, probing it first if it has not.
    pub(crate) async fn facts_or_probe(&self, machine: &Machine) -> Result<Facts, ExecError> {
        match self.facts_this_run(&machine.name) {
            Some(facts) => Ok(facts),
            None => self.probe(machine).await,
        }
    }

    /// Run `command` on `machine`. The first command on a machine in a run probes it first, for
    /// its login `PATH` (and to tell an unreachable machine from a failing command).
    pub async fn exec(
        &self,
        machine: &Machine,
        command: &str,
        start: &StartDir,
        timeout: Option<Duration>,
        lines: Option<mpsc::Sender<Line>>,
    ) -> Result<Run, ExecError> {
        let facts = self.facts_or_probe(machine).await?;
        let script = ExecScript {
            command,
            start,
            path: facts.path.as_deref(),
        }
        .render();
        self.connection(machine).exec(script, timeout, lines).await
    }

    /// Put `bytes` in `file_name` under `dir` on `machine` (made if missing), and return the
    /// file's absolute path there. The bytes travel inside the script, base64 in a here-document,
    /// so no shell reads them as code and no argument limit applies.
    pub async fn upload(
        &self,
        machine: &Machine,
        dir: &RemoteDir,
        file_name: &str,
        bytes: &[u8],
    ) -> Result<String, ExecError> {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        let mut body = String::with_capacity(encoded.len() + encoded.len() / 76 + 512);
        for chunk in encoded.as_bytes().chunks(76) {
            body.push_str(&String::from_utf8_lossy(chunk));
            body.push('\n');
        }
        let script = crate::script::upload(dir, file_name, &body);
        let stdout = self
            .connection(machine)
            .run_short(&script, Duration::from_secs(120))
            .await?;
        stdout
            .lines()
            .find_map(|line| {
                line.strip_prefix(crate::script::MARK)?
                    .strip_prefix("uploaded ")
            })
            .map(str::to_owned)
            .ok_or_else(|| ExecError::Unreachable {
                machine: machine.name.clone(),
                destination: machine.ssh.clone(),
                reason: "it did not confirm the upload".to_owned(),
            })
    }

    /// Where a command on `machine` starts when the agent did not say and no earlier command
    /// there left a directory: this project's path under the machine's home (when the project
    /// is under this computer's home), then the configured directory, then the home directory.
    pub fn default_dirs(&self, machine: &Machine, local_cwd: &Path) -> Vec<RemoteDir> {
        let mut dirs = Vec::new();
        if let Some(relative) = dirs::home_dir()
            .and_then(|home| local_cwd.strip_prefix(home).ok().map(Path::to_path_buf))
            .filter(|relative| !relative.as_os_str().is_empty())
        {
            dirs.push(RemoteDir::Path(format!("~/{}", relative.display())));
        }
        if let Some(cwd) = &machine.cwd {
            dirs.push(cwd.clone());
        }
        dirs
    }

    /// The machine names a schema may list, `local` first.
    pub fn names(&self) -> Vec<String> {
        std::iter::once(LOCAL.to_owned())
            .chain(self.machines.keys().map(ToString::to_string))
            .collect()
    }

    /// One line per machine: name, platform, home, and what it is for.
    pub fn describe(&self) -> Vec<String> {
        self.machines
            .values()
            .map(|machine| {
                let mut line = format!("- {}", machine.name);
                match self.reach(&machine.name) {
                    Some(Reach::Reachable(facts)) => {
                        line.push_str(&format!(": {}", facts.platform()));
                        if !facts.home.is_empty() {
                            line.push_str(&format!(", home {}", facts.home));
                        }
                    }
                    Some(Reach::Unreachable { reason }) => {
                        line.push_str(&format!(": unreachable at the last check ({reason})"));
                    }
                    None => line.push_str(": not checked yet"),
                }
                if let Some(description) = &machine.description {
                    line.push_str(&format!(". {description}"));
                }
                line
            })
            .collect()
    }
}

/// Shared ssh connections live in Nori's home, or in `~/.nori/ssh` when that path is too long
/// for a unix socket name: one connection per machine, not one per command, which also keeps
/// sshd's penalties for many short connections (OpenSSH `PerSourcePenalties`) away.
fn control_dir(nori_home: &Path) -> Option<PathBuf> {
    // ssh appends `/` and a 40-character hash, then a temporary suffix while it binds.
    let fits = |dir: &Path| dir.as_os_str().len() + 64 <= 104;
    let dir = Some(nori_home.join("ssh"))
        .filter(|dir| fits(dir))
        .or_else(|| dirs::home_dir().map(|home| home.join(".nori").join("ssh")))
        .filter(|dir| fits(dir))?;
    std::fs::create_dir_all(&dir).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).ok()?;
    }
    Some(dir)
}

/// A machine that did not answer is asked again at startup only after this long; using it asks
/// at once.
const RETRY_UNREACHABLE_SECONDS: i64 = 6 * 3600;

/// The roster of this process: installed once at startup, empty until then (and in tests).
#[derive(Clone)]
enum Global {
    NotStarted,
    Discovering,
    Ready(Arc<Roster>),
}

fn global_slot() -> &'static tokio::sync::watch::Sender<Global> {
    static SLOT: OnceLock<tokio::sync::watch::Sender<Global>> = OnceLock::new();
    SLOT.get_or_init(|| tokio::sync::watch::channel(Global::NotStarted).0)
}

fn empty() -> Arc<Roster> {
    static EMPTY: OnceLock<Arc<Roster>> = OnceLock::new();
    Arc::clone(EMPTY.get_or_init(|| Arc::new(Roster::empty())))
}

/// Discover the user's machines on a thread of their own and install them as this process's
/// roster. [`global_ready`] waits for it; nothing blocks a thread on it.
pub fn discover_in_background(nori_home: PathBuf) {
    let started = global_slot().send_if_modified(|slot| match slot {
        Global::NotStarted => {
            *slot = Global::Discovering;
            true
        }
        Global::Discovering | Global::Ready(_) => false,
    });
    if !started {
        return;
    }
    let runtime = tokio::runtime::Handle::try_current().ok();
    std::thread::spawn(move || {
        let roster = Arc::new(Roster::discover(&nori_home));
        install(Arc::clone(&roster));
        // Learn what each machine is, once, so agents are told before they need one.
        if let Some(runtime) = runtime
            && !roster.is_empty()
        {
            runtime.spawn(roster.probe_stale(Duration::from_secs(7 * 24 * 3600)));
        }
    });
}

/// Make `roster` this process's roster.
pub fn install(roster: Arc<Roster>) {
    global_slot().send_replace(Global::Ready(roster));
}

/// This process's roster as it is now, never waiting: empty until discovery has finished (and
/// when it never started). For code that cannot wait, such as building a tool list.
pub fn global() -> Arc<Roster> {
    match &*global_slot().borrow() {
        Global::Ready(roster) => Arc::clone(roster),
        Global::NotStarted | Global::Discovering => empty(),
    }
}

/// This process's roster once discovery has finished (a few seconds at most); at once when it
/// has, or never started. For async code that is about to use a machine.
pub async fn global_ready() -> Arc<Roster> {
    let mut slot = global_slot().subscribe();
    let ready = tokio::time::timeout(
        Duration::from_secs(4),
        slot.wait_for(|slot| !matches!(slot, Global::Discovering)),
    )
    .await;
    match ready {
        Ok(Ok(slot)) => match &*slot {
            Global::Ready(roster) => Arc::clone(roster),
            Global::NotStarted | Global::Discovering => empty(),
        },
        Ok(Err(_)) | Err(_) => global(),
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn roster(config: &str, rex_hosts: Vec<rex::RexHost>) -> Roster {
        let config: ConfigFile = toml::from_str(config).unwrap_or_default();
        Roster::from_sources(rex_hosts, config.machines)
    }

    fn name(text: &str) -> MachineName {
        text.parse().unwrap_or_else(|_| unreachable!("valid name"))
    }

    fn destination(text: &str) -> SshDestination {
        text.parse()
            .unwrap_or_else(|_| unreachable!("valid destination"))
    }

    #[test]
    fn config_describes_reroutes_and_hides_rex_hosts() {
        let rex_hosts = vec![
            rex::RexHost {
                name: name("box"),
                route: RexRoute::Ssh(destination("me@box.ts.net")),
            },
            rex::RexHost {
                name: name("prod"),
                route: RexRoute::Alias(destination("prod")),
            },
            rex::RexHost {
                name: name("mac"),
                route: RexRoute::ThisComputer,
            },
        ];
        let roster = roster(
            r#"
            [machines.box]
            description = "build box"
            cwd = "~/src"
            [machines.prod]
            hidden = true
            [machines.gpu]
            ssh = "me@gpu"
            "#,
            rex_hosts,
        );
        assert_eq!(
            roster.machines().cloned().collect::<Vec<_>>(),
            vec![
                Machine {
                    name: name("box"),
                    ssh: destination("me@box.ts.net"),
                    description: Some("build box".to_owned()),
                    cwd: Some(RemoteDir::Path("~/src".to_owned())),
                    nori: NoriThere::default(),
                    jobs_dir: None,
                    origin: Origin::RexSsh,
                },
                Machine {
                    name: name("gpu"),
                    ssh: destination("me@gpu"),
                    description: None,
                    cwd: None,
                    nori: NoriThere::default(),
                    jobs_dir: None,
                    origin: Origin::Config,
                },
            ]
        );
        assert_eq!(roster.resolve(Some("mac")), Ok(Target::Local));
        assert_eq!(roster.resolve(None), Ok(Target::Local));
        assert_eq!(
            roster.resolve(Some("prod")).map_err(|error| error.to_string()),
            Err("there is no machine named `prod`; the machines are `local` (this computer), box, gpu".to_owned())
        );
        assert_eq!(roster.names(), vec!["local", "box", "gpu"]);
    }
}
