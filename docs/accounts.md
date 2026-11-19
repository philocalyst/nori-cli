# Accounts

An **account** is one login of one **provider**: a home (the directory its credentials live in) and
a name. Running two Claude subscriptions, or a work and a personal Codex, is two accounts of one
provider. Nori treats them as a concept of its own instead of as unrelated agents.

Code: `nori-config/src/accounts.rs` (schema and the provider table),
`nori-rs/acp-host/src/accounts/` (the model, wrappers, unwrapping, identity, creating accounts).

## Three kinds

| Kind | Where it comes from | Runs | Slug |
| --- | --- | --- | --- |
| default | the built-in agent as it ships | the provider's own login | `claude-code`, `codex` |
| native | an `[[accounts]]` entry | in Nori's process, one registration per account, no wrapper script | `codex@work` |
| wrapper | an `[[agents]]` entry whose launcher script points the provider at its home | the launcher, as a subprocess | the entry's own slug (`claude2`) |

A wrapper is read, never rewritten: `accounts::list()` scans a `local` distribution's launcher (and
the one it hands over to) for the provider's home variable, so `nori-claude2-acp`, which names the
`claude2` launcher that exports `CLAUDE_CONFIG_DIR=…/profiles/secondary`, is "Claude, secondary".
Only small `#!` text scripts are read, only their `export` lines are used, and no credential file
is opened.

## The provider table

One table (`nori_config::PROVIDERS`) says how a provider is told which account to use. A provider
with no variable keeps one login in one place and has a single account.

| Provider | Home variable | Native account |
| --- | --- | --- |
| Claude | `CLAUDE_CONFIG_DIR` | yes: the in-process agent drives the `claude` CLI with it |
| Codex | `CODEX_HOME` | yes: the in-process engine takes the home as its configuration directory |
| Gemini | `GEMINI_CLI_HOME` | when the build has `native-gemini` |
| OpenCode | `XDG_DATA_HOME` (`<home>/data`) | when the build has the OpenCode agent |
| Antigravity | none | no: single account |

## Config

```toml
[[accounts]]
provider = "codex"       # claude | codex | gemini | opencode
name = "work"            # the agent is `codex@work`, shown as "Codex · work"
# home = "~/vault/codex-work"   # default: $NORI_HOME/accounts/codex/work

[[accounts]]
provider = "claude"
name = "client"

# Unchanged: a wrapper agent is read as an account of Claude.
[[agents]]
name = "Claude 2"
slug = "claude2"
[agents.distribution.local]
command = "nori-claude2-acp"
```

On load each `[[accounts]]` entry becomes one more agent with a `native` distribution
(`nori_config::expand_accounts`), so `--agent codex@work`, `default_models`, fallback chains and
the model picker see it like any other agent. A name is up to 32 letters, digits, `-` or `_`. An
unknown provider, a provider with a single account, or a duplicate slug is a config error naming
the entry.

## How a native account starts

The account's home reaches the agent only through the agent's own spec, never the process
environment, which every in-process agent shares:

1. `get_agent_config("codex@work")` builds an `InProcessSpec` with `home` set and an environment from
   `accounts::native_environment`: the provider's home variable, and for a provider that runs an
   executable (Claude), that executable **unwrapped**.
2. `in_process::with_environment` folds it, with the provider's variable for the home, into the
   spec and runs the registration's resolver.
3. The agent uses it: Codex's `load_agent(home)` makes the home its `codex_home`; Claude's driver
   starts its CLI with the environment as the CLI's environment.

### Unwrapping

The user's `claude` is a launcher: it exports `CLAUDE_CONFIG_DIR` for one profile and execs the real
program. Run as-is, a native account would silently land in that profile whatever home Nori set. So
`accounts::unwrap_executable(provider, command)` follows `exec <program> <args> "$@"` lines (at
most two hops), keeps the launchers' other exports, and drops the provider's home variable. The
account then runs the real program with its own home.

A launcher is followed only when it is plain: `export NAME=value`, `set -…`, one `exec … "$@"`.
Branching, `unset`, command substitution and anything else Nori would have to evaluate make the
answer `None`, and "Add another account" says exactly why the account is unavailable instead of
guessing where the script would land. The launcher's extra arguments (`--mcp-config`,
`--dangerously-skip-permissions`) are not given to the in-process Claude CLI: Nori supplies its own
MCP servers and owns permissions.

## `/providers`

`/providers` manages accounts; models are chosen in `/model`. Accounts are grouped under their
provider (the one in use first, then those signed in, agents of no provider last), each on one line:
who it is signed in as and how much of its tightest rate limit window is left (`robert@example.com ·
Max · 78% of 5h left · resets in 2h`), or `not signed in`, or `not installed`. An account whose
agent says it is signed out is not signed in, whatever it listed before. Choosing an account opens
its page: **Sign in** (or **Sign in again**), and where its login lives, what starts it and its
agent slug. Code: `tui/src/nori/providers_panel.rs`.

The last row, **Fallback order**, shows where a session goes when a request fails, read from the
Pkl policy (`$NORI_HOME/init.pkl` over `default.pkl`; see [workflows.md](workflows.md)). Pkl stays
the only place fallbacks are written: a route's `agent` is an account's slug
(`new Dynamic { agent = "claude@work"; model = "opus" }`), and **Edit in init.pkl** names the file,
creating it first with an empty chain that lists every account as a commented-out route.
Code: `tui/src/nori/fallbacks_panel.rs`.

## Adding an account from `/providers`

`accounts::plan_add(provider)` answers what "Add another account" does:

- `Native`: ask for a name; `accounts::add_native_account(nori_home, provider, name)` creates
  `$NORI_HOME/accounts/<provider>/<name>/` (mode 0700), appends the `[[accounts]]` entry to
  config.toml (comments and other entries untouched), and registers the agent in the running
  registry. Sign-in is the next step, for that account.
- `Unavailable(reason)`: the provider has a single account, the build has no built-in agent for it,
  or its launcher cannot be followed. A wrapper that sets its own home gets named
  ("`nori-codex-acp` sets `CODEX_HOME` itself") with what to do: add another launcher in the nix
  config that sets the variable to the new directory, and list it under `[[agents]]`.

## Signing in

Sign-in runs through the agent for that account (`acp-host/src/sign_in.rs`), so it lands in that
account's home: an agent that advertises a method signs in through `authenticate` (Codex's
`chatgpt`) or its terminal method. An in-process agent has no launch command for a terminal method,
so when it offers no method Nori can run, `accounts::login_program(slug)` is its terminal method: the
provider's own login CLI (`claude auth login --claudeai`, `gemini login`, `opencode auth login`), run
through the unwrapped executable with the account's home, in the sign-in panel. `/login`'s own table
is used only for an account that keeps its login where the provider does by default. "Signed in" is said only once a fresh agent for that account opens a session
(`sign_in::verify`), and a failed sign-in names the account.

## Identity

An agent says who it is signed in as with the `_auth/status_update` notification (the shape the
Claude adapter's `authStatus` extension pushes, `nori_acp_kit::AuthStatus`): a kind, a label, and
for a subscription the email, organization and plan. The ACP host files it under the slug of the
agent the connection was started for, so `claude2` and `claude-code` stay separate, in memory and
in `$NORI_HOME/accounts/identity.json`. `accounts::identity_of(slug).describe()` is the line
`/providers` and the status line show: `robert@example.com · Max`. Only what the agent said is
kept; a credential never reaches Nori's identity store.

In-process agents send the same notification: Claude from `claude auth status --json` run with the
account's home, Codex from the ChatGPT login's own email and plan.

## Usage and routing

The slug is the account's identity in `harness/src/usage/` and `recovery/`: a native account is its
own `AccountId` (provider `codex`, label `work`, key `agent:codex@work`), counted, shown in
`/usage`, and compared by headroom as "Codex · work". A reported `_meta.nori.account` (an email)
still wins, so a native account and a wrapper signed in to the same login pool their windows. See
[usage.md](usage.md).
