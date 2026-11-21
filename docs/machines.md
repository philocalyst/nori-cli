# Machines and portable sessions

Nori runs commands on the user's other computers, and moves a whole session to one of them, without
an agent ever typing `ssh` or `rex`. Execution lives in `nori-machines`; the `bash` host tool, jobs
and session moves are built on it.

## The roster

A **machine** is one of the user's computers that Nori may run commands on, named by a
**machine name**. `local` always means the computer Nori runs on.

| Source | What Nori takes |
| --- | --- |
| Rex hosts (`rex hosts --json`) | Every host. A host with an `ssh://` endpoint is reached there. A host Rex reaches over HTTPS is reached over ssh by its label, which works when the user's ssh config has that alias. A host whose HTTPS endpoint is this computer's own Rex server is `local`. Rex labels are shared by every device on the same Rex home, so a name means the same computer everywhere. |
| `[machines.NAME]` in `config.toml` | Adds a machine (`ssh = "user@host"`), or describes, re-routes or hides a Rex host of the same name. |

```toml
[machines]
rex_hosts = true                 # import Rex hosts (default)

[machines.h16003mac]
description = "fast build mirror; ~/nori-build has a checkout"

[machines.gpu]
ssh = "me@gpu.example.com"       # host, user@host, or an ssh config alias
cwd = "~/src"                    # where commands start when nothing better is known

[machines.ilo-prod]
hidden = true                    # never offered to agents

[machines.mothership]
jobs_dir = "~/scratch/nori-jobs" # where jobs keep their files there (default ~/.nori/jobs)
nori = "~/bin/nori"              # Nori there, for moves, when it is not on the login PATH
nori_home = "~/.nori/cli"        # its NORI_HOME, when not the default
```

The roster is read once per process, off the UI path, at startup. A **probe** (one ssh round trip)
learns each machine's OS, architecture, home and login `PATH`; facts are kept in
`$NORI_HOME/machines.json`, so agents are told what each machine is before Nori connects to it. A
machine not probed for a week is probed again at startup, in the background.

## Running a command on a machine

The `bash` host tool takes `machine` (an enum of `local` and the roster's names, each described with
its platform, home and purpose) and `cwd`. Agents are told the machines and when to use them in the
host tools' instructions.

- **Transport.** ssh, never Rex: Rex's API cannot run a command without opening a pane on that host,
  and returns its output only as a captured screen. Nori runs `ssh -T -o BatchMode=yes HOST /bin/sh
  -s` and sends one self-contained `sh` script on stdin, which every login shell (bash, zsh, fish,
  nu) runs the same way. Connections are shared (`ControlMaster`, sockets in `$NORI_HOME/ssh`).
  Credentials never pass through Nori: ssh uses the user's own keys and agent, and a machine that
  would prompt is reported unreachable.
- **Shell.** Each call starts a fresh bash (else `sh`) with the machine's login `PATH`. The directory
  carries over between calls (per caller, per machine); exports, functions and aliases do not. The
  first call starts in the project's path under that machine's home when it exists, then the
  configured `cwd`, then home. A `cwd` the agent names must exist, or nothing runs.
- **Results.** The same status line and body as a local run, naming the machine (`exit 0 on
  mothership`). An unknown name lists the known ones. An unreachable machine says why and that
  nothing ran. The TUI's shell cell shows `on NAME`.
- **Lifetime.** A command on a machine runs on the job substrate from the start (below): detached
  on the machine, its output appended to files there and streamed back. A dropped connection only
  interrupts the streaming, which reconnects. When the command outlives `timeout_ms`, the call
  returns what it printed so far and the command carries on as a background job whose end wakes
  the agent; it is never killed for being slow. A command that ends in time leaves nothing behind.
- **Background.** `background: true` starts the command as a job and returns at once (`watch`, a
  regex, also wakes the agent on matching lines). The agent never polls; it ends its turn.
- **This computer.** A command in the persistent local shell that outlives `timeout_ms` keeps
  running in that shell, followed as an in-process job (it ends with this Nori). The caller gets a
  fresh shell in the same directory that took over the exports, aliases and functions it had
  before the command; the result says so, and that unexported variables, options, traps and what
  the command changed after it started did not carry over.
- **Restrictions.** An actor whose `bash` a program restricts by command and path runs on this
  computer only.

## Jobs

A **job** is a supervised background process on any machine ([pkl-runtime.md](pkl-runtime.md#jobs)
for the tools and wakes). The supervisor (`nori_machines::jobs`) starts it detached (its own session
and process group via `setsid`, or `perl POSIX::setsid` on a Mac), with its output appended to
`~/.nori/jobs/ID/{out,err}` and its end written to `exit` on its machine. A **cursor** is a byte
offset into each stream, so reads are exact across reconnects, restarts and moves. A job record
names its machine and that machine's boot, so a restart that reused the pid reads as `lost`. Stopping
sends TERM to the group and KILL after a grace. Environment values are passed in memory and never
written to any machine's disk.

## Portable sessions

A **move** takes a session, whole, to another machine and continues it there live. It is one
operation (`session.move { machine }` in the plugin API; `/move NAME` in the TUI; the Rex plugin's
command) and has four steps.

1. **Plan** (instant, nothing changes). Resolve and probe the target; check that Nori is installed
   there and reads this bundle version; find the project there (the same repository by its remote,
   at the mapped path, the configured `cwd`, or a path the user names) and compare its commit with
   this one; check that the session's agent and account exist there. The plan lists consequences in
   one line each: the agent's own session restarts there with the conversation handed over (prompt
   cache lost), an account that is missing there falls back to that provider's default account,
   jobs stay on the machine that runs them.
2. **Fence.** At the next turn boundary (or at once, if the user cancels the turn) the session stops
   taking prompts, monitors are paused, and the journal and transcript are flushed and closed.
3. **Ship.** The **session bundle** goes to the target over the same ssh connection and is imported
   by `nori session import` into the target's Nori home, filed under that home's project for the
   target directory. Then the origin's journal records the move (`moved-out`), a marker beside the
   transcript (`ID.moved`) says where it went and how to reach it, and the origin's resume list
   shows the conversation as "moved to NAME". A conversation that moves back is imported over its
   old copy and the marker goes.
4. **Hand over.** A launcher is staged on the target (`incoming/ID.resume.sh` in its Nori home:
   the login `PATH`, the directory, `nori resume ID`), so any login shell runs it as
   `/bin/sh LAUNCHER`. In Rex, a session on the target's Rex host runs it (`rex -S HOST new`) and
   the user's app is asked to show it (`session.select`; without Rex's Remote Control the person is
   told to open it from the sidebar), and this Nori exits. Outside Rex, Nori exits and this terminal
   runs `ssh -t TARGET /bin/sh LAUNCHER`. On the target, the session resumes live, adopts its jobs,
   re-arms its monitors, and re-applies its permissions.

`nori session export ID`, `nori session import BUNDLE --cwd DIR` and `nori session move ID MACHINE`
do the same from the command line for a conversation that is not running. A machine where Nori is
not on the login `PATH` names it: `[machines.NAME] nori = "~/bin/nori"`, `nori_home = "..."`.

### What travels

| In the bundle | How it is used on the target |
| --- | --- |
| Transcript and conversation journal (JSONL and Pkl), with every delegated child's | The conversation, unchanged; the agent's new session gets it as a handoff. |
| Agent, account name, model, effort, mode | Applied as dials; a missing account or model is reported once, never blocks. |
| Approval policy, saved approvals and "always allow" rules | Re-applied. Path rules are rewritten from the origin's project root to the target's. |
| Job records | Adopted where they run; an origin `local` job becomes a job of the origin machine. |
| Monitors | Re-armed, with paths mapped. |
| The working tree's state: branch, `HEAD`, unpushed commits (a git bundle) and uncommitted changes (a binary diff) | Applied to the target checkout when it is clean and at the same base; otherwise the plan says what differs and the user decides. |

**Never in a bundle:** credentials of any kind (agent logins, API keys, ssh keys, keychain items,
`auth.json`), environment values, and the agent's own session store. The target uses its own
sign-ins; the user is told if an account is not available there.

After an import from another known machine, Nori restores delegated children from their
conversation journals once and records that recovery. Ordinary resumes on the same machine
do not replay those child journals.

The **conversation ID** survives a move. The ACP session ID does not; see
[transcripts.md](transcripts.md).
