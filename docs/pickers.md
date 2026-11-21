# Pickers: the Helix engine, `/resume` and the actor panel

Every list-with-preview in Nori is one engine, `HelixState` in
`nori-rs/tui-components/src/picker/helix/`. Pickers own only their rows and their
previews; the engine owns the frame, the query language, ranking, selection, scrolling,
mouse and keys, so every picker looks and behaves the same.

Pickers draw inline on the primary screen, never the alternate screen. They are
overlays on the live region, not separate views: closing one returns to the chat
exactly as it was.

## Frame and layout

```text
╭─ ◇ Sessions ───────────────────┬────────────────────────────────────────────────╮
│ ❯ cancel @agent:codex  █ 2 / 6 │ ~/Projects/nori-cli · codex · gpt-6    Hit 1/3 │
├────────────────────────────────┼────────────────────────────────────────────────┤
│ ▸ ● Fence WakeTurn cancel… 12m │ ▌ Fence WakeTurn delivery so a cancel never…   │
│     2 hits · …peek the fence…  │                                                │
╰─ indexing 12 / 98 ──────────── ┴──────────────────────────────────── 2.8ms ─────╯
```

- The list takes 38% of the width and the preview the rest. Below 78 columns, or after
  `Tab`, the preview is hidden and the list takes the box.
- The prompt is `❯ query`, with the count of matching rows over all rows (`2 / 6`) at
  the right. A query wider than its row shows its end.
- The bottom edge carries a status on the left (`indexing 12 / 98`) and the time the
  last search took on the right. Pickers show no key hints; the keys below are the same
  everywhere.
- Rows are drawn by the picker (one or two lines each). The selection is an item, not a
  position: rows that appear, end or reorder around it leave it where it is.

## Query language

`auth @agent:claude @branch:feat/x %running !@model:haiku`

- Plain words are free-text terms; every term must match. Smart case: a term without an
  uppercase letter ignores case.
- `@key:value` filters the field `key` (substring, smart case). A bare `@value`, `%value`
  or `#value` filters the field bound to that sigil: `@` agent and `%` status in the
  actor panel, `@` agent in `/resume`. `!` before the value negates the filter.
- A token that names no known field stays ordinary text. A filter with no value yet
  (`@agent:` while typing) constrains nothing.

Ranking: a term's score is its best field's fuzzy score minus that field's penalty (the
title has none), so a title match outranks a match in agent, branch or model. Fuzzy
matches spread thinly across a long text are rejected. A term that is in none of the
fields can still match the item's full text (see the sections below); those rows rank
behind field matches and show `N hits · …context…`. An empty query keeps the natural
order.

## Keys and pointer

| Input | Effect |
| --- | --- |
| text, Backspace, Ctrl-W, Ctrl-U | edit the query (Ctrl-U clears it) |
| Up / Down, Ctrl-P / Ctrl-N | move the selection, wrapping |
| PageUp / PageDown, Home / End | page, first, last |
| Tab | show or hide the preview |
| Ctrl-D / Ctrl-F, Ctrl-B | scroll the preview |
| Alt-N / Alt-P | next / previous match inside the preview |
| Enter | choose the selected row |
| Esc | clear the query, then close |
| Ctrl-C | close and, where the host can, quit (the startup `/resume` picker exits Nori) |
| Space | in a multi-select picker with an empty query, toggle the row |
| wheel | over the list moves the selection; over the preview scrolls it |
| click | chooses that row |

Pickers can bind `Alt+<key>` to actions on the selected row. An action that needs text
(`compose`) turns the prompt into `✉ message luna-wake ❯ …` until Enter sends it or Esc
cancels.

## Sections, multi-select and choices

- A row can belong to a **section**; while the query is empty a heading is drawn above each
  run of one section, and above the first visible row of a scrolled list. A filtered list is
  flat.
- A picker can be **multi-select**: rows carry a gutter (`◼` toggled, `◻` not), `Space`
  toggles the selected row while the query is empty, and Enter submits every toggled row.
- A row can be **disabled**: it is listed with its reason and can be highlighted, and Enter
  leaves it alone. Rows keep the host's order while the query is empty; the engine never
  re-sorts a list (ranking is the host's, for example the harness `RouteRanking`).
- A row can **ask** for text first (`Custom…` in the loop count picker): Enter turns the
  prompt into `iterations ❯ …`, and the host accepts the text or answers with a reason that is
  shown while the prompt stays.
- `ChoiceSpec` (`tui/src/choice_picker/`) describes such a list as data and the app shows it
  as an overlay (`AppEvent::ShowChoicePicker`). Every `SelectionViewParams` with the picker
  presentation is converted to one, which is how the model, config, skillset, fork and
  provider pickers use the engine. Browsing fires nothing; only choosing runs an action.

- **Dial effects.** A picker that sets the model, the effort or the mode never refuses a choice
  and says what it does: `ChatWidget::dial_effect(dial, value, turn_running)` answers synchronously
  from the options the agent last advertised with a `nori_protocol::DialEffect` (when it applies and
  what it costs). It is the effect the choice declares in `_meta.nori.dial` (see `native-agents.md`),
  else `NextTurn` with no costs. A model the current agent does not list belongs to another agent: it
  is `AfterThisTurn` while a turn runs, else `Now`, with `SessionRestarts`.

- **Query modes.** A picker can have its query always open (default), closed until `/`, `f` or
  Ctrl-F opens it (`on_demand`; Esc closes it, `j` and `k` move while it is closed), or none.
  With `numbered`, a digit chooses the row of that number while no query is being typed.
- **Bottom-pane lists.** `ListSelectionView` (approvals, simple popups) keeps its API,
  `ListSelectionView::new(SelectionViewParams, tx)`, and its look; the engine is its model
  (filtering, selection, sections, digits, `/` search) and `HelixState::visible` hands it the
  rows to paint.

## Host contract

A picker is a `HelixItem` type plus a host that keeps items current, feeds input, and
answers what the engine lacks, never from the draw path:

- full-text hits: after the query's text changes, `take_search_request` returns the
  terms; the host searches wherever the text lives and answers `set_hits` with the
  request's generation (answers for an older query are dropped);
- previews: `preview_request` names the selected item, the width and the query terms
  when the preview is stale (another item, another width, a new revision, a live item's
  250 ms refresh); the host answers `set_preview`.

## `/resume`

One picker, one list, for `nori resume` and for `/resume` inside a session. Every row resumes
into a live session you can type into; there is no read-only view of a past session.

A capture of `nori resume` at 80 columns (Rex, a scripted world's two conversations), then
the same picker searching the conversations' content for `cookie`:

```text
╭─ ◇ Sessions ────────────────┬────────────────────────────────────────────────╮
│ ❯   search sessions   2 / 2 │ claude-code · opus                             │
├─────────────────────────────┼────────────────────────────────────────────────┤
│ ▸ ● Fence WakeTurn del… now │ ▌ Fence WakeTurn delivery so a cancel wins     │
│     1 turn · claude-code    │                                                │
│   ● Fix the flaky logi… now │ A cancel now wins over a pending wake: the     │
│     1 turn · claude-code    │ turn is fenced before delivery.                │
╰─ claude-code · this project ┴────────────────────────────────────────────────╯

╭─ ◇ Sessions ────────────────┬────────────────────────────────────────────────╮
│ ❯ cookie              1 / 2 │ claude-code · opus                     Hit 1/1 │
├─────────────────────────────┼────────────────────────────────────────────────┤
│ ▸ ● Fix the flaky logi… now │ ▌ Fix the flaky login test                     │
│     1 hit · …n cookie. I m… │                                                │
│                             │ The login test raced the session cookie. I     │
│                             │ made it wait for the redirect; it passed 50    │
│                             │ times in a row.                                │
╰─ claude-code · this project ┴──────────────────────────────────────── 0.0ms ─╯
```

- **What is listed.** Nori's own record of each conversation (its transcript) in the current
  project, most recent *activity* first (when anything was last recorded, not when the session
  began, so a conversation resumed an hour ago is above one started this morning). The age on
  the right is that activity. Delegated children (subagent and program sessions) are never
  listed: they are not conversations of their own and belong to the session that delegated them.
  A session in which nothing was asked is not listed.
- **Scope.** `nori resume` lists every agent's sessions of the project; `--agent X` only that
  agent's; `--all` every project's. `/resume` lists the active agent's sessions of the project,
  because only that agent (and account: `codex@work` is not `codex`) can reopen them. The
  bottom edge says which: `claude-code · this project`. Ctrl-A switches between that list and
  all chats (every project's sessions, of every agent), and again switches back.
- **Agent-listed sessions.** When the active agent lists its own sessions (`session/list`),
  those that Nori has no record of (started outside Nori) are in the same list, ordered by
  their own update time. A session Nori recorded is never shown twice, whatever agent session
  ids it has run in. When a candidate agent is being switched to, or on a cloud broker, the list
  is only what that agent lists, under a pinned "Start a new session" row.
- **The current session** is marked `◆ … current`; choosing it leaves things as they are.
- **Moved conversations.** A conversation that moved to another machine (see
  [machines.md](machines.md#portable-sessions)) says `moved to NAME` among its facts; it
  continues there.
- **Keys** are the engine's (see above): type to filter, Up/Down, PageUp/PageDown, Home/End,
  Enter resumes, Esc clears the query, then backs out. Backing out inside a session returns to
  the chat. At startup it **leaves Nori without starting a session**, like Ctrl-C: `nori resume`
  means "resume something", so Esc is "never mind", not "start fresh". There is no mode to
  switch: the list is the same however it was opened.
- **Empty and no-match states.** An empty list says `No sessions recorded yet for <scope>.`
  inside `/resume`; at startup, `nori resume` and `nori resume --last` with nothing to resume
  print `No recorded sessions to resume in <directory>. Run nori to start one, or nori resume
  --all to look in every project.` and exit instead of starting an unrelated session. A query
  that matches nothing says so and how to widen it.
- **Nothing to remember.** The picker keeps no state of its own between openings: the list is
  rebuilt from the records every time, so it is never out of date, and the query always starts
  empty. What persists is the session (below).

The picker opens within the 100 ms an interaction may take. Its rows come from each session's
first record (the session's metadata, read the same however long the session grew) and from the
index, which knows the title, branch, model, turn count and the agent the session runs on now
(a provider handoff changes it). The newest sessions are brought up to date first, for at most
40 ms, which is usually enough because an update reads only what a session grew by; the rest fill
in as the index catches up, and never reorder. The sessions the agent lists itself (`session/list`)
are asked for at the same time as Nori's records: the picker does not wait for them, and adds
them when they arrive. A preview of a session not indexed yet says so for as long as that session
takes to index, and then shows it; that session is indexed ahead of the rest. Searching matches those fields at once
and, on a worker, the full text of every session; a session whose words are in its conversation
shows `N hits · …snippet…`. The preview is the session's conversation (user turns on a rail,
assistant prose, tool calls with highlighted commands) opened at the first match, with `Hit i/N`
in the title row.

A session's title is what the person asked: the title the agent gave it
(`session_info_update`), else the first user message. Nori names every session from its first
prompt as soon as it is sent (a `session_info_update` carrying `_meta.nori.titleSource =
"prompt"`, journaled like any other), because the Claude and Codex adapters never send a
title while they run; an agent's own title, when it sends one, replaces it. A resumed
conversation keeps its title: when the session starts, Nori says again the title it last had
(`titleSource = "recorded"`; for a conversation recorded before titles, one derived from its
first prompt), whether Nori or the agent replays its history, and the first prompt after the
resume does not rename it. The footer's session title (on by default), the terminal's tab
(`project · title`), the status card, the program status record's `title` and the pickers all
show it. Everything Nori injected around a
prompt (the tags in `harness/src/injected_context.rs`: `<context>`,
`<nori_agent_catalog>`, `<goal_context>`, `<workflow>`, checkpoints and so on) is removed
from titles, from the digest's user turns (so it is not searched or previewed either), and
from titles an agent lists.

The second line of a row is what tells sessions apart: the working directory when it is
not the one Nori runs in (`~` for home, cut from the left so the end of the path stays),
then branch, turn count and agent.

### What a resume keeps

A resume continues the session it was chosen from:

- **The conversation record.** A resumed session goes on recording into its own transcript
  (same session id, same row in the picker), and its last activity moves it to the top. Quitting
  a session finishes its index digest (waiting at most 1.5 s), so the next picker has its title.
- **The agent and account.** `nori resume` starts the agent that recorded the session, whatever
  the configured default is (`--agent` only narrows the list). Resuming a `codex@work` session
  starts `codex@work`.
- **The agent's own session**, when the agent still has it: `session/resume` (or
  `session/load`), which keeps the model, effort and mode as the agent holds them.
- **When the agent no longer has it** (`session/resume` or `session/load` fails, or the agent
  supports neither) the conversation continues anyway: a new session of the same agent and
  account in the same directory, the old conversation drawn on screen from the transcript, and a
  summary of it given to the agent as context with your first message. The model, effort and mode
  the conversation last had are put back where the new session offers them. You are told once, in
  one line, why. A session only the agent knew (no transcript) continues as a new, empty session
  with a notice saying so.

## The full-text index

`$NORI_HOME/index/v1/{project-id}/` holds, per session, four files; the implementation is
`nori-rs/harness/src/session_index/`. Each is written for what an update of a session that
grew has to change, so an update costs what was appended, not the length of the session.

- `.nidx` is the turns that are final, JSON lines: one merged turn per line (role, text,
  and for tool calls the command and the first 600 characters of the output). It carries
  what a person remembers, not protocol traffic, and it also feeds the picker's preview,
  so a preview never re-parses a raw transcript. It is only ever appended to. A turn is final
  when nothing later can touch it: a text turn when another turn follows, a tool turn when its
  call finished (or someone spoke after it). A merged turn of text is at most about 64 KiB; a
  longer run starts another turn.
- `.nhd` is the header (ids, start time, cwd, agent, model, branch, title, user-turn count,
  and the source stamp): small, replaced whole, and written last, because it is what makes an
  update visible.
- `.tri` is a bloom filter of the session's lowercase character trigrams. A search
  loads the filters of all sessions, discards every session that cannot contain a term,
  and reads only the remaining digests. Terms shorter than three characters are not
  searched on disk. New text sets a few more bits, and only the bytes those bits fall in are
  patched in the file. A filter that is more than 30% full is built again, larger, from the
  digest; each doubling follows a doubling of the session's distinct trigrams, so building
  again costs a constant per appended byte.
- `.nst` is the cursor: which file the session was read from (the `.pkl` journal, or the
  `.jsonl` of a session recorded before journals), how many bytes of it, how many bytes of
  `.nidx` are valid, and the extractor's state there: the few turns that are not final yet
  (the last turn, which may still grow, and tool calls that can still receive output), the
  title, model and bindings so far. It does not hold what is final, so it stays small however
  long the session is.
- A digest is the header, the turns of `.nidx` up to the length the cursor names, and the
  cursor's unfinished turns, so the last turn is searchable and previewed too.
- Updates are incremental. A session's records only grow at the end (the journal and the
  JSONL transcript are append-only). Bringing a digest up to date reads only the bytes
  appended since, and writes only what they added: turns that became final are appended to
  `.nidx` (after truncating any bytes an interrupted update left past the length the cursor
  names), the filter is patched, then the cursor and the header are replaced. The session
  someone is in is current after reading what its last turns added, however long it is; a
  digest whose cursor is at the file's end is current without reading anything. Only a rewind
  or a fork, which change which earlier records are visible, make the journal be read from its
  start again. A missing or unreadable cursor means the session is read from its start once.
  The picker syncs when it opens, and the recorder syncs a session's digest when the session
  ends. Digests of deleted transcripts are removed. Small files are written to a temporary
  name and renamed, without a disk flush (the index is derived: a digest that did not survive
  a power loss fails to read and is rebuilt); a digest with an unknown `schema` is rebuilt.
- Every entry point is `async` or meant for `spawn_blocking`; nothing runs on the UI
  thread. The index is derived data and can be deleted at any time.

## The actor panel

The actor panel (`Ctrl+Space` by default) is the same picker over the live tree of the
main session and every subagent:

```text
◈ main
├─ ● luna-wake        8s · gpt-6-luna
│  └─ ● proptest-runner
└─ ✓ deepseek-audit   6s · deepseek-v4
```

- Rows are named by the **subagent name** of [the glossary](glossary.md#how-the-tui-names-and-counts-subagents),
  the same name the lanes and summary cards use; `●` is running (a spinner while animating), `○` idle.
- The tree is ordered running first (most recent activity on top, fixed from when it
  was first seen), then ended ones, most recently ended first. While a query is active
  the rows are ranked and flat, each naming its parent (`parent › child`).
- Fields: title (the task), `@agent`, `model`, `%status` (`running`, `done`, `failed`,
  `cancelled`, `idle`), `parent`, `detail`. The full text of each actor's conversation
  is searched in memory.
- The preview is the actor's conversation, following its end while the actor runs.
- Enter focuses the actor in the chat; the panel closes. `Alt+M` messages the actor,
  `Alt+S` steers it (interrupts its turn, then delivers), `Alt+X` stops its turn. These
  go through the same harness controls the chat uses, and the panel stays open so
  several actors can be handled in a row.
