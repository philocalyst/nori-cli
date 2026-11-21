# Accounts

An **account** is one login of one **provider**: a home (the directory its credentials live in) and
a name. Running two Claude subscriptions, or a work and a personal Codex, is two accounts of one
provider. Nori treats them as a concept of its own instead of as unrelated agents.

Code: `nori-config/src/accounts.rs` (schema and the provider table),
`nori-rs/acp-host/src/accounts/` (the model, wrappers, unwrapping, identity, creating accounts, the
ways to sign in in `login.rs`, verifying a login in `status.rs`).

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
| Codex | `CODEX_HOME` | yes: Nori's engine reads the home as Codex does (`config.toml`, `auth.json`, model cache); refreshed ChatGPT tokens are written back to it |
| Gemini | `GEMINI_CLI_HOME` | when the build has `native-gemini` |
| OpenCode | none | when the build has the OpenCode agent: the account's API key is kept in its home (`<home>/opencode-api-key`) |
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

### Prefer an account when routes share a model

`[account_preferences]` in `config.toml` orders account agent slugs within one provider, without
changing another provider's route order:

```toml
[account_preferences]
claude = ["claude@client", "claude-code"]
```

An active named account remains first, while the implicit built-in default yields to the configured
order. When Nori ranks sibling accounts for the same model, the configured order comes before
headroom; an account known to be spent still goes after available accounts when `prefer_headroom` is
enabled. Explicit routes in the recovery chain keep their order. Nori cannot distinguish an
explicit choice of the built-in account from that account being the implicit default. This is a
routing preference, not a replacement for selecting the session's agent with `agent`.

## How a native account starts

The account's home reaches the agent only through the agent's own spec, never the process
environment, which every in-process agent shares:

1. `get_agent_config("codex@work")` builds an `InProcessSpec` with `home` set and an environment from
   `accounts::native_environment`: the provider's home variable, and for Claude, whose agent runs
   an executable, `claude` **unwrapped** and handed over as `CLAUDE_CODE_EXECUTABLE`.
2. `in_process::with_environment` folds it, with the provider's variable for the home, into the
   spec and runs the registration's resolver.
3. The agent uses it: Codex's flavor makes the home its `CODEX_HOME` (login, configuration and
   catalog cache; sessions stay in Nori's `agents/codex/`); Claude's driver
   starts its CLI with the environment as the CLI's environment; OpenCode's reads the API key kept
   in that home (no OpenCode program runs; see [native-agents.md](native-agents.md)).

### Unwrapping

The user's `claude` is a launcher: it exports `CLAUDE_CONFIG_DIR` for one profile and execs the
real program. Run as-is, a native account would silently land in that profile whatever home Nori set. So
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
Max · 78% of 5h left · resets in 2h`). How an account stands is **verified, never assumed** (see
[Verifying a login](#verifying-a-login)), and is one of:

| Row says | Meaning |
| --- | --- |
| `f@example.org · Max` | the account's own login was found, as that identity |
| `not signed in · <why> · Enter to sign in` | its home holds no working login |
| `checking…` | a check is running: the first since Nori started, or the one every opening of the panel starts. The last known identity is named as "last seen"; the previous verdict is never shown as if it were current |
| `can't tell if it is signed in · <why>` | the check could not run (the CLI is missing, it hung), or it ran and proved nothing (Antigravity opened a session that listed no models) |
| `not installed · <command>` | there is nothing to run, and this is the one command that installs it |

Opening the panel starts one check per account on a task; the rows redraw as each answer lands (a
frame is asked for with each, so the open panel does not wait for a key), and the UI never waits on
one. Choosing an account opens its page: **Sign in** (or **Sign in again**, or **How to install** when there is nothing to run),
and where its login lives, what starts it and its agent slug. Code:
`tui/src/nori/providers_panel.rs`.

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
account's home. Choosing **Sign in** lists the ways the account has, each run against that account's
home; a way that needs something asks for it first. What cannot be typed into a program is asked in
the picker (an email; a key or token, shown as dots), and a code a browser shows is pasted into the
sign-in panel above the prompt, which has its own input line (Enter sends it to the program).

| Provider | Ways |
| --- | --- |
| Claude | **Claude subscription** (`claude auth login --claudeai`: browser, or paste the code it shows); **with your email** (asks for it, `--email`); **SSO** (asks for the work email, `--sso --email`); **Anthropic Console** (`--console`); **long-lived token**, either made in the panel (`claude setup-token`; Nori keeps the token it prints) or pasted; **API key** (pasted) |
| Codex | **ChatGPT** in the browser (the agent's `authenticate`); **device code** (`codex login --device-auth` for this home, offered when the `codex` CLI is installed); **OpenAI API key** (pasted; handed to the agent for that one sign-in, which stores it in the home's own `auth.json`) |
| Gemini | what the Gemini CLI offers over ACP (`authenticate`: Login with Google, which opens the browser; Vertex AI). Its **API key** method and its gateway are not offered: a key is read from `GEMINI_API_KEY` in the environment Nori starts the CLI with (`authenticate` keeps only the choice of method, not the key), so the picker's subtitle says where it goes. The CLI has **no `gemini login` command** (Nori used to run one, which could only fail), so when it offers nothing `/login` says to set `GEMINI_API_KEY` or the Vertex AI variables. Google moved personal accounts (AI Pro and Ultra, free Code Assist) to Antigravity, so the CLI's Google login is for work accounts; `/login` and the panel say so |
| Antigravity | what `agy acp` offers over ACP (`authenticate`: **Log in with Google** and **Gemini Enterprise**, which open the browser themselves and answer once the sign-in is done; **Gemini API key** and **Agent Platform** (Vertex AI), which need configuration outside Nori, and fail with the CLI's own words), and `agy acp --login`, the command the official CLI names itself: Google sign-in in the browser, or over SSH the authorization URL and the code it shows, pasted into the panel |
| OpenCode | **OpenCode API key** (pasted; from opencode.ai/auth, handed to the agent for that one sign-in, which keeps it in the account's home, readable by its owner only) |

A token or key for Claude is kept only for an account Nori added: it is written to the `env` of
`settings.json` in that account's home (mode 0600, other settings untouched), the place Claude
documents for `ANTHROPIC_API_KEY` and `CLAUDE_CODE_OAUTH_TOKEN`. The default login's settings are the
user's own and are not written. Nori never reads or prints a credential file; it asks the provider
who is signed in.

"Signed in" is said only once the account's own login says so (`sign_in::verify`), and a failed
sign-in names the account and the reason. Adding an account (`/providers`, "Add another account")
creates it and signs it in the same way; an account whose sign-in was abandoned stays "not signed
in" in `/providers`. `/login`'s own table is used only for an account that keeps its login where the
provider does by default and offers no way above.

## Verifying a login

`accounts::status::check(slug)` (`acp-host/src/accounts/status.rs`) asks the account's provider
about that account's home, and answers `SignedIn(identity)`, `NotSignedIn(reason)` or
`Unknown(reason)`:

- **Claude**: `claude auth status --json` run as the account runs Claude (the real executable with
  the account's `CLAUDE_CONFIG_DIR`, so a launcher that pins another profile cannot answer for it).
  A logged-out Claude exits non-zero but still answers; the answer decides, not the exit code. A key
  or token login reports its source, not an email, and is only known to be present: nothing is sent
  to Anthropic, so a key that was revoked still reads as signed in until it is used.
- **Codex** and **OpenCode**: the in-process agent's auth probe (`Registration::with_auth_probe`),
  which reads that home's stored sign-in (OpenCode's: its kept key) without starting a session. Codex also reads the ChatGPT
  access token's own expiry after letting `AuthManager` renew one that is near it, so a login whose
  refresh token was refused is "not signed in", not "signed in".
- **Gemini** and **Antigravity** keep their login in the CLI's own store (a file, the OS keyring)
  that Nori never opens, and have no status command, so the check is what the CLI does itself: start
  its ACP agent and open a session (`session/new`). Both refuse it without a login, with the ACP
  authentication-required error `-32000`, in their own words: `agy acp` 1.5.x says "Authentication
  required" (its log: "Silent sign-in failed; session requests fail with auth_required until the
  client signs in"), `gemini --acp` 0.47 says "Gemini API key is missing or not configured." That is
  "not signed in", and the words are the row's reason. A session that opens is "signed in", though
  neither says as whom; for Antigravity it must also **list models** (its `configOptions` carry a
  `model` select, which a signed-in `agy acp` fills, and which the CLI leaves empty when its sign-in
  mode is unspecified), and a session that lists none is "can't tell". Nothing is read from a credential store and no prompt is
  sent. A signed-in Gemini that was given a key reads as signed in whether or not the key is valid:
  the CLI does not check it until it is used. This starts the agent (a Node CLI, a worker), so these
  rows take longer to settle. A CLI that is missing, or too old to speak ACP, is "can't tell" with
  the reason the registry gave, never "signed in".

### The Gemini CLI

`gemini` is looked for on `PATH`, then in the usual install locations (`NPM_CONFIG_PREFIX/bin`,
`~/.npm-global/bin`, `~/.local/bin`, Bun, Volta, pnpm, mise and asdf shims, nvm's node versions,
nix profiles, Homebrew), because the `PATH` of a nix-wrapped or GUI-launched Nori is often not the
shell's (`accounts/gemini_cli.rs`). One that is found is started directly (`gemini --acp`); without
one, Nori starts it through `npx` or `bunx` when it has one. Detection is not cached, so installing it
while Nori runs is noticed on the next open of `/providers`.

When there is nothing to run, the row says `not installed · <command>`, where the command suits the
machine: `npm install -g @google/gemini-cli` when npm is there, else `brew install gemini-cli`, else
`nix profile install nixpkgs#gemini-cli`, else "install Node.js, then run npm install -g …". The
page adds what else works: personal Google accounts sign in through Antigravity (and says whether it
is installed and signed in), and a Gemini API key or Vertex AI variables (`GEMINI_API_KEY`) work
once the CLI is installed. Nori's own Gemini engine (`nori-gemini-acp`, feature `native-gemini`) is
a Node process driving Gemini Core, not an in-process Rust engine, and takes only an API key or
Vertex AI, so it is not a way around installing anything.

A check spends no model usage. The answer is filed in the identity store (`not signed in` replaces a
remembered identity) and held for this run, so rows can draw it again.

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
