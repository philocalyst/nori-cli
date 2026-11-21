# nudox and impact assessments

[nudox](https://nudox.org) is local-first, versioned code intelligence: it indexes a project into a
graph of declarations and answers where something is used, what calls it, and what implements it,
with the exact source span that proves each answer. Nori includes it by default and ships a skill,
`impact`, that turns it into an assessment of what a change affects. Terms follow
[glossary.md](glossary.md).

## On by default

When the `nudox` command is on `PATH`, every session of every agent kind gets nudox's MCP server for
the session's project (its working directory), with the entry `nudox mcp-config --format json
--project <cwd>` prints (a stdio server; its first tool call starts nudox's local daemon). Nothing is
indexed by this: the server only answers, and says so when the project is not indexed.

| Rule | Behavior |
| --- | --- |
| Where | One harness function, `nudox::add_to` (`nori-rs/harness/src/backend/nudox.rs`), called by every path that builds a session's MCP list: a new session, a resume or load, and a session swap (compaction, rewind). Delegated children and program actors get it the same way. |
| Never delays | The entry is resolved in the background, once per project, and cached for the life of the process. Resolution starts when the backend is prepared, long before a session exists. A session waits for what is left of 100 ms counted from that start, then goes on without nudox; the next session of that project has it. A missing, failing or hung `nudox` (5 s limit) is retried after 60 s. |
| Not installed | Nothing is added and nothing is shown. |
| Opt out | `nudox = false` at the top level of `config.toml` (default `true`). |
| Your own entry | A server named `nudox` under `[mcp_servers]`, enabled or not, is yours: Nori adds nothing. |

Agents are told about it in the tool guidance every session gets: use nudox instead of grepping to
learn where something is used, and **before changing a public declaration, ask for its impact** by
running the `impact` skill.

## The `impact` skill

`impact` is a built-in Pkl skill ([pkl-runtime.md](pkl-runtime.md#built-in-modules)). An agent runs
it with `skill` (`name: "impact"`); a person runs `/impact`. With no arguments it assesses the
working tree's changes (staged, unstaged and untracked, against `HEAD`); `declarations` names
declarations to assess instead.

It reports, in this order: the changed declarations (signature or body, with their coordinates), their
direct users, their transitive users (bounded by `depth`, default 2 hops beyond direct, and `limit`,
default 25 rows per declaration and hop, with what was cut off counted), the tests that reach them, risk
notes, and what is not proven. Every item cites the nudox call and coordinate that proves it and, for a
use, its file and lines; what came from the diff alone is marked `[diff]` or `[unproven]`.

**Indexing.** If the project is not indexed the report says so. Asking for an impact assessment is
the agent's consent to index this project, and the skill then indexes it (`index = yes`, the default)
and says it did; with `index = no` it only offers. It never indexes another path, a dependency or a
parent directory.

The analysis runs in one actor (`agent`, default `claude-code` on `sonnet`; another agent uses its
own default model) that is read-only by instruction and uses the nudox MCP tools, or the `nudox`
command line when the tools are not offered. Change a built-in skill by copying it to
`$NORI_HOME/skills/impact.pkl` or `.nori/skills/impact.pkl`.

## What nudox does not do

There is no single impact command in nudox: `references` and `graph` answer one declaration at a
time. The skill is the walk over them. nudox shows the project as it was indexed, so a change made
since then is judged from the diff and flagged; users outside the project (published items) are
invisible and listed as a risk.
