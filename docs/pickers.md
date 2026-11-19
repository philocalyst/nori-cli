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

The `/resume` picker lists the recorded sessions of the project for the active agent.
Rows appear from transcript metadata at once. In the background the full-text index
(below) is brought up to date, newest session first, and rows fill in with the first
message as title, branch, model, agent and turn count. Searching matches those fields
immediately and, on a worker, the full text of every session; a session whose words
are in its conversation shows `N hits · …snippet…`. The preview is the session's
conversation (user turns on a rail, assistant prose, tool calls with highlighted
commands) opened at the first match, with the matches highlighted and `Hit i/N` in the
title row. Enter resumes the session; for an agent that lists its own sessions
(`session/list`) the rows come from the agent and have no full-text index.

## The full-text index

`$NORI_HOME/index/v1/{project-id}/` holds, per session, `{session-id}.nidx` and
`{session-id}.tri`; the implementation is `nori-rs/harness/src/session_index/`.

- `.nidx` is JSON lines: a header (ids, start time, cwd, agent, model, branch, title,
  user-turn count, and the source stamp) and then one merged turn per line (role, text,
  and for tool calls the command and the first 600 characters of the output). It carries
  what a person remembers, not protocol traffic, and it also feeds the picker's preview,
  so a preview never re-parses a raw transcript.
- `.tri` is a bloom filter of the session's lowercase character trigrams. A search
  loads the filters of all sessions, discards every session that cannot contain a term,
  and reads only the remaining digests. Terms shorter than three characters are not
  searched on disk.
- A digest is rebuilt only when its transcript's length or modification time differs
  from the source stamp (`.pkl` and `.jsonl` together). The picker syncs lazily when it
  opens, and the recorder syncs a session's digest when the session ends. Digests of
  deleted transcripts are removed. Both files are written to a temporary name and
  renamed; a digest with an unknown `schema` is rebuilt.
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
