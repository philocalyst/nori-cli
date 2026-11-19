# Usage

What agents spend, kept on disk, and the `/usage` view that shows it.

## Reporting (contract)

An in-process agent attaches a usage report to `_meta.nori` of an ACP `usage_update` (or a
`session_info_update` when no usage update fits):

```json
{ "nori": { "model": "claude-haiku-5-5", "account": "you@example.com",
            "tokens": { "input": 120, "output": 40, "cacheRead": 900, "cacheWrite": 0, "reasoning": 0 },
            "rateLimits": [ { "id": "five_hour", "label": "5h", "usedPercent": 41.0,
                              "resetsAt": "2026-11-18T23:00:00Z", "windowMinutes": 300 } ] } }
```

- `tokens` are the **delta** since the agent's previous report. `"tokensScope": "session"` marks
  them cumulative for the session instead; the harness differences them and treats a count that
  went down as a reset.
- `reasoning` is already part of `output`; it is carried only for display.
- `rateLimits[].id` is stable per provider and windows merge by it. `kind` is `session`,
  `weekly`, `monthly` or `other` and defaults from `windowMinutes`.
- `account` names the account the quota belongs to. Sessions that report the same `(provider,
  account)` **pool** their windows: the most used open report stands for the window and the view
  shows how many sessions reported it. See [Accounts](#accounts) for who an unreported account is.
- ACP's own `UsageUpdate.cost` is cumulative session cost in USD; the harness records its delta as
  the provider-reported cost.
- Types: `UsageReport`, `UsageTokens`, `RateLimitWindow` in `nori-protocol`
  (`UsageReport::from_meta`).

## Storage

`nori_harness::usage::UsageTap` (one per root session, fed from the session relay) turns reports
into samples; `UsageWriter` appends them off the session's task to
`$NORI_HOME/usage/YYYY-MM.jsonl` (UTC month, one JSON object per line). Samples hold only what
is new. Rate-limit windows are written when they change. Subagent sessions are recorded with their
`root_session_id`. A line that does not parse is skipped when reading.

Cost is the provider-reported cost when there is one. Otherwise it is priced when the summary is
read, so a changed price applies to old samples. A model with no price is *unpriced*: its tokens
count, its cost does not, and the view says so.

## Accounts

Usage and windows belong to a provider **and an account**, never to an agent name or a model
alone, so two Claude subscriptions never pool and one is never mistaken for the other. An
account is, best source first:

1. the `account` the agent reported;
2. the last account the same agent reported, for a report that omits it;
3. the agent's slug, which names its profile: `claude-code` is Claude `primary`, `claude2` is
   `secondary`, `claude3` `tertiary` (the number in the slug). Two slugs that would both be
   `primary` keep their slug in the name.

A **gateway** (`opencode`, `openrouter`) is billed by the provider of the model it routed to, so
`anthropic/claude-opus-5-5` through `opencode` is an account of Claude named `via opencode`, and
`opencode-go/deepseek-v4` one of DeepSeek. It never merges with a direct account of that
provider. When a gateway and a native agent report the same tokens (same root session, same
provider, identical totals within ten seconds) the gateway's copy is dropped, so a delegated
child is not counted twice.

`nori_harness::usage::AccountId` carries `provider`, `via`, `label` and `agents`; `UsageSummary`
has `accounts` (spend per account and model) beside `limits` (windows per account). A window is
pooled per session by its newest report, so a report that mentions only some windows leaves the
others as they were.

## Routing by room

`nori_harness::recovery::AccountHeadroom` answers, from the windows the usage log last saw,
what an account has left: `room(agent, provider)` and `room_for(&RouteRef)` give a `Room`
(percent left of the tightest window, which window, when it resets, whether it is spent),
`best(&routes)` picks the route whose account has the most, and `describe(&route)` names the
account (`Claude · secondary`). `AccountHeadroom::load(nori_home)` reads the last eight days of
windows only and is cached for five seconds, so a picker may call it.

`RouteRanking::with_headroom` makes ranking use it: among routes to one model, accounts of the
same provider trade places by room (coarse tiers: 50% and up, 20% and up, below), a spent
account (under 3% left) last. Ranked fallback, `/model` and `RouteChanged` (which carries
`from_account` and `to_account`) all use it. The knob is `recovery.fallback.prefer_headroom`
([workflows.md](workflows.md#recovery)). An account with no windows reported has unknown room,
which counts as plenty. In `/model`, a row reachable through several accounts lists the account
with room first with a five-cell meter (`▰▰▰▰▱ 78% left · also on Claude ▱▱▱▱▱ spent`).

## Prices

A built-in table of approximate list prices covers the common Claude, GPT/Codex, Gemini, DeepSeek
and Grok models. Override per model in `$NORI_HOME/config.toml` (USD per million tokens):

```toml
[usage.prices."claude:claude-sonnet-4-5"]   # "<provider>:<model>", or a bare model id
input = 3.0
output = 15.0
cache_read = 0.3     # optional, defaults to input
cache_write = 3.75   # optional, defaults to input
```

## `/usage`

An overlay on the primary screen. Opening it and switching period read the store on a task; the
UI never waits for the disk.

Overview, top to bottom:

- Header: period `24h` (hourly buckets) `7d` `30d` (daily buckets), the provider filter (`all
  providers` or one provider), `prices`, and the metric `cost` / `tokens`. Every chip is
  clickable.
- Headline total, session count, per-provider rows with a trend line.
- Totals: processed, cached input, uncached input (input plus cache writes), output, cache
  savings.
- Stacked chart per provider over the period.
- Limits: grouped by provider, then account; accounts never pool. A provider with several
  accounts leads with the headroom across them (`Claude · 2 accounts · 41% / 12% of 5h used`),
  then one row per account with its windows side by side (5h, week), each a meter, used % and
  reset countdown, and a pace mark where the window length is known. The account with clearly
  the most room is marked `most room`. Where the width cannot fit columns, each window gets a
  row of its own with `ahead of pace` (used exceeds the share of the window that has passed by
  10 points) and `pooled · N sessions`. Amber from 75%, coral from 90%. Windows whose reset
  time has passed read `refilled`. The first line, `Tightest`, is the compact summary: the
  window with the least room among the accounts that spent anything in the period, and the room
  a sibling account of the same provider has (`UsageSummary::most_constrained`).
- Accounts: shown only for providers that split across accounts or gateways. Cost and tokens per
  account, and per model beneath it; a gateway route is its own account (`Claude · via opencode`).
- Share bars by model and by token type.
- Model table with share, tokens and `custom price` on overridden models. The cursor moves
  through it; Enter opens the model.

Model detail (Enter on a row, or a click): cost or tokens headline with sessions and share,
tokens and cost by type with a share bar, the model's own chart over the period, the price in
effect (list price or custom), cache savings, and the `config.toml` table to add to override it.

Prices (`prices`, `r` or `$`): the effective price table for every model seen in the period,
then the overrides nothing has used yet. Read-only: prices are edited in
`$NORI_HOME/config.toml`, never in the view.

Keys: `d` `w` `m` or Tab period, `p` provider filter, `t` metric, `r`/`$` prices, Up/Down or
`j`/`k` move the model cursor (above the first row they scroll), PgUp/PgDn, Home/End and the
mouse wheel scroll, Enter opens the model, Esc goes back (and closes from the overview), `q`
closes. Colors come from `Theme` tokens only.
