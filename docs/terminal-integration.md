# Terminal integration

Nori tells its terminal what it is doing so that a terminal that shows many programs at
once (Rex, in particular) can show the conversation and each subagent without guessing from
the screen. The channel is the
[Program Status Protocol](https://www.superlogical.com/rex/docs/build/program-status),
`OSC 7501`, revision 0.4. Presentation (badges, sounds, notifications, grouping) is the
terminal's; Nori reports facts.

`nori-rs/tui/src/terminal_status/` owns every byte of the protocol: encoding and limits
(`wire.rs`), detection (`probe.rs`) and the outbox (`mod.rs`). `app/program_status/` decides
what to say, from the session's events. Nothing else writes `OSC 7501`.

## Detection

At startup, after raw mode is on and before the event reader exists, Nori sends
`OSC 7501 ; ? ST` followed by primary device attributes (`CSI c`) and reads the replies from
the controlling terminal. A status reply (`states=...:kinds=...`) enables reports and names
what the terminal accepts; a device-attributes reply first means the protocol is unsupported,
and Nori never writes a report. A terminal that answers neither within 250 ms is treated as
unsupported. A reply without `states` or `kinds` means all of them; a state the terminal lacks
falls back to the nearest one it has (`blocked` to `working`, `done` and `error` to `idle`),
and a `kind` it lacks is left out.

`NORI_PROGRAM_STATUS` overrides detection: `off` (also `0`, `false`, `no`, `never`) sends
nothing; `on` (also `1`, `true`, `yes`, `always`) sends without asking. Anything else asks.

## Records

The root record is the conversation. Each subagent is a child record whose id nests like the
subagent tree: `s1`, `s1/s2`, at most eight levels and 255 children. Every report carries
`app=nori` and repeats everything the record keeps, because a report replaces its record.

| Record | `state` | When |
| --- | --- | --- |
| root | `idle` | At rest, after a cancel, after a session starts, after the user has seen a result |
| root | `working` | A turn runs (also a turn the agent started itself, a retry wait) |
| root | `blocked` | A permission request (`kind=permission`), a question (`kind=question`, or `auth` when the question asks for a secret), a sign-in (`kind=auth`) |
| root | `done` | The turn ended; `msg` says how long it took and what it did |
| root | `error` | The request failed, the agent refused, or the session was lost; `msg` is the reason |
| child | the same | The child's own turn, requests and outcome |

`title` is the conversation's label (the agent's session title, else the title Nori derived from the first prompt, else
the prompt summary, else the project directory) and, for a child, what Nori calls it (its task, else its type; the type
is the working `msg` until a tool runs). `msg` is one line of plain text: the tool that is
running, `Thinking`, `Writing a response`, `Retrying in 5s (2/5)`, `No activity for 90s`, the
request being asked (`Approve Run cargo test?`, the question), or the finished-turn summary
(`Finished in 1m 12s · 5 tool calls · 2 subagents`). `progress` is the share of the agent's
plan that is completed, while it works or is blocked.

A record blocked on several requests shows the newest, with `(+N more)`. A child's block is its
own: the root stays `working` or `idle`.

## Lifecycle

- A new session (`/new`, resume, switch, route swap) clears every record and starts again with
  the root `idle`. A sign-in in progress survives it.
- A subagent appears when it is announced, is `working` while it runs, `idle` between turns and
  after a cancel, `done` or `error` when it ends, and is cleared with its descendants when the
  agent reports it closed.
- `done` and `error` stay until the user presses a key (the root goes `idle`, finished children
  are cleared) or the next turn starts.
- Quitting, a closed session and `/undo` clear. Ctrl+Z, a panic and every hand-back of the
  terminal (`restore`) withdraw all records at once; after a resume every record is said again.
  A killed process leaves nothing stale: the terminal drops `working` and `blocked` records
  when their process exits.
- Replayed history reports nothing but titles.

## Writing

Reports are queued in the status outbox and leave inside the frame writer's next write, in
front of the frame, so one never lands inside a frame. Between frames the app flushes the
outbox after each event. State changes go out at once; changes to `msg` or `progress` alone are
held to one report per record every 500 ms, the newest wins, and a report identical to what the
terminal holds is never sent. Free text is stripped of control and invisible characters,
collapsed to one line and cut to the protocol's limits (`title` 192 bytes, `msg` 2048).

Nori does not emit `OSC 133` prompt marks: the live region is not a shell prompt, and terminals
treat prompt lines specially on resize.

## The window title

Every terminal also gets the plain window title (`OSC 0`, `nori-rs/tui/src/terminal_title.rs`):
`project · title`, the project directory's name and the conversation's title, the same title the
footer, the pickers and the root record's `title` show (see [pickers.md](pickers.md)). Before
the conversation has a title it is the project alone. It changes when the title does, and is
cleared when Nori exits.

## Clicks on Nori's links (Rex)

Nori never turns on mouse reporting over the conversation, so the terminal's own selection always
works. A click reaches Nori another way: Nori draws `nori://TOKEN/...` OSC 8 links (a subagent's
name in its lane opens the subagent; `TOKEN` is this process's, so a link left in the scrollback by
another Nori opens nothing), and Rex reports clicks on links as events of the pane.

- **The event.** When a client forwards a click on a link's cells, the Rex server emits the block
  event `url_clicked {url}` ("the server never opens the link; the client decides"). Clicks off the
  link emit nothing, and the program in the pane receives no input.
- **Following it.** In a Rex pane (`REX_SESSION`, `REX_BLOCK` set, the server's CLI in
  `REX_BIN_DIR`), Nori runs `rex events -s $REX_SESSION --json` (`nori-rex`) and takes the
  `url_clicked` events of its own block. It reconnects with backoff when the stream ends and stops
  when its pane closes. Links are drawn only while this subscription is live
  (`nori_link::set_routed`).
- **The gesture.** The Rex app opens links on Cmd+click; a plain click is selection and is not
  forwarded to the server. What the app does with a custom scheme after the click follows the
  user's **Links That Open Apps** setting (`client.setting.terminal.links.custom_schemes`: ask,
  always, never).
- **OSC 133 `click_events`** is not used: Rex delivers nothing to the program for clicks on marked
  prompt rows.
