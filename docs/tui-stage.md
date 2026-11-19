# TUI Stage

How the Nori TUI draws the conversation on the primary screen, what leaves it for the terminal's
scrollback, how a prompt is focused for rewinding, and how input is routed. Terms: **stage**,
**tail**, **boundary**, **focus** (see [glossary.md](glossary.md)).

## One screen, never the alternate one

Nori never enters the alternate screen and never takes the whole screen away from the
conversation: not for pickers, not for the transcript pager, not for child conversations. The
conversation is the terminal's own text. Its wheel scrolls the terminal's scrollback, its
selection copies, and its links open with the terminal's own click, so the mouse is never
captured over it; only a transient panel (a popup, a picker, a child's conversation) takes the
mouse while it is open.

The **live region** starts where the program started; above it is the shell's own output and the
conversation that already scrolled off. Inside it sit the **tail** of the conversation and, under
it, the **bottom block**.

The bottom block has a resting part, the composer and its status line, and a transient part:
the working indicator, a running tool, the answer still streaming, the lanes of running
subagents, a popup or an open view. Only the resting part decides what the conversation hands to
the scrollback. The transient part sits on top of it and clips the tail from the top instead:
the clipped rows are still the stage's, and they come back the moment the transient part
shrinks. So nothing is ever drawn under a popup, and nothing leaves the screen for good because
something live was tall for a while: the composer never floats above a gap.

Pickers, the pager, the actor panel, `/usage` and a child's conversation are **panels**: they
take the bottom of the screen (three fifths of it, all of it only on a very short screen) with
the conversation still above them, and closing one leaves the screen as it was.

### Links

Both writers (the frame diff and the rows typed into the scrollback) write URLs and the paths of
files that exist as OSC 8 hyperlinks, found in the text itself (`tui/src/hyperlink.rs`). They
work in the scrollback and after Nori exits. A URL wrapped across rows is one link on both rows.
`NORI_NO_HYPERLINKS` turns them off.

### Resuming

A resumed conversation brings its last screens into the scrollback (at least 2,000 rows), not
its whole past: rendering every cell of a long history would hold the resume up for rows the
terminal does not keep anyway. A line marks where it starts, and the transcript view (`Ctrl+T`)
has everything.

## The stage

`tui/src/stage/` owns what the region shows and what leaves it.

- The tail is the cells of the conversation that are not in the scrollback yet. They are drawn
  like any other widget (hard-wrapped rows in the frame's cell grid, cached per width), so a
  running tool, an expanded card or a focused prompt changes in place.
- The **boundary** is the first row that is not in the scrollback: a cell index and how many of
  its rows are committed. Rows leave the tail from the top, and only when the region is full: the
  screen scrolls, the rows that pass the top edge are in the scrollback from then on, and the
  boundary moves past them. A row is committed exactly once because nothing draws it again once
  the boundary has passed it.
- Two ways to commit. When the rows leaving are the ones on screen, the screen scrolls (line
  feeds on the last row, the one scroll every terminal keeps history for). When they are not (a
  long history appearing at once, or rows that changed since they were drawn) the stage types the
  rows from the top of the region, so they pass through the screen in order.
- While the tail is *decorated* (a prompt is focused, a tool card is expanded) it is clipped at the
  top instead of scrolling, so a view the user opened never pushes rows away.
- Content flows from where the program started; the bottom block follows it until the region is
  full, then stays at the bottom. A transient part of the block clips the tail instead of
  pushing it into the scrollback (see above).
- Only onboarding and other screens that are not the conversation ask for a block as tall as the
  screen: the tail is neither drawn, scrolled nor committed meanwhile, and the next ordinary frame
  draws it again.

Every frame is one synchronized update written with one `write`.

### Clear screen

`Ctrl+L` pushes everything on screen into the scrollback, whole (it scrolls; `ESC[2J` and
`ESC[3J` are never sent) and starts a clean region with the composer at the top.

### Resize

A resize must not cost the visible conversation. The terminal moves the screen by itself: a
shorter screen pushes the top rows into the scrollback so the cursor stays on screen, a taller
one may pull rows back (iTerm2, kitty, Ghostty, WezTerm do; xterm does not). How the cursor moved
says by how much, so the frame after a resize starts from where the cursor is.

1. The event stream, right after it is handed `Event::Resize` (when nothing is waiting on the
   input), asks the terminal where the cursor is (`CursorReport::query`, `ESC[6n`) and leaves the
   answer for the next frame (`ResizeSignal`). A reader that is already waiting would take the
   reply, which is why it is asked here and not from a frame. A terminal that does not answer is
   not asked again.
2. `Presenter::recover_from_resize` compares it with where the last frame parked the cursor. The
   difference is how many rows the terminal pushed up (negative: pulled down). The shell's rows
   above the region went first; the rest of the push came off the top of the tail, and those rows
   are marked committed (`Stage::note_lost_rows`), so none is drawn again. A pull moves the
   region down by as many rows.
3. The rows from the first row of the region down are erased, one row at a time (never `ESC[J` at
   the top of the screen, which tmux reads as "clear the screen" and pushes everything into the
   scrollback, and never `2J`/`3J`), and the tail is drawn again at the new width from there, down
   to the composer. Rows above the region, and the scrollback, are not touched: the terminal
   reflows them itself.
4. Without an answer, the shift is assumed to be the push a shorter screen needs (or the pull a
   taller one allows) and nothing is marked committed: a row may then be in the scrollback and on
   screen at once, or an old row stay above the redrawn tail. That is preferred to a screen that
   lost its context.

A focused prompt, a picker or a pager is a transient view: it is redrawn at the new width, and
its rows are never marked committed.

## Focus-backwards rewind

One model for walking back to a prompt (`app/focus.rs`). `Esc` on an empty composer while idle
(a running turn is interrupted instead) focuses the last prompt; `[` in normal mode does too.
The prompt becomes an amber card with a `↶ Turn n of N` header, a rollback marker follows it, and
everything after it is dimmed.

The keys are entries of the keymap (below), so this table is the default:

| Key | Does |
|---|---|
| `[` `]`, `↑` `↓`, `k` `j` | move between prompts; past the newest, leave |
| `Enter`, `e` | edit the prompt in place; `Enter` sends it, restoring the files of the turns it rolls back |
| `r` | rewind: restore the files, fork before the prompt, put it in the composer |
| `R` | the same, keeping the files |
| `f`, `Space` | fork a subagent from its response |
| `[t` | toggle the tool card nearest the composer, and leave |
| `Esc` | leave |

While subagents run and the agent is idle, the first `Esc` on an empty composer says "Esc again
stops N running subagents · [ rewinds" and a second one within two seconds stops them; `[` always
walks back.

Every rewind ends in `App::fork_conversation_at`: the conversation is cut before the prompt, the
agent starts a new session told what came before, and the prompt returns (composer or sent).
The same model walks a child's journal, where the actions are the journal's rollback and fork.

## Status line

One row under the composer: a mode badge (`INS`, `NOR`, `REWIND 2/3`, `BASH`, `RESUME`,
`ACTORS`), the agent and model, and on the right the branch, cost, context percent and running
actors. No key hints; `?` lists the keys.

## The keymap

Every key the TUI answers itself is one entry of `nori_config::Keymap`: a mode (global, focus,
composer), an action, its bindings and a line of help. Behaviour (`app/input`), the `?` list
and this document read from the same table. Rebind an entry by id under `[tui.keymap]`:

```toml
[tui.keymap]
"focus.rewind" = ["r", "ctrl+r"]
"global.toggle_tool" = "alt+t"
```

Entries that stand for a `[tui.hotkeys]` binding (clear screen, transcript, editor, actor panel)
follow that binding; the composer's typing prefixes (`!`, `/`, `@`, `?`) are fixed and listed so
the `?` list is complete. Unknown ids and fixed entries are ignored.

## StageSim

`stage/sim.rs` (test builds) runs the stage on an in-memory terminal: a `vt100` screen with
scrollback that moves rows on a resize like a cursor-following terminal. `behave` picks what
terminals differ on (`TerminalBehavior`): whether a taller screen pulls rows back from the
scrollback, and whether the cursor query is answered. Frames are presented by
the real `Presenter`, the code `Tui` runs minus raw mode and synchronized-update markers, so a
sequence of conversation changes, keys and resizes can be replayed and read back:

```rust
let mut sim = StageSim::after_shell(60, 16, &["~ ❯ nori"]);
sim.say("MARK01");                  // a prompt and its answer
sim.stream(&["streaming..."]);      // a live tail above the composer
sim.escape();                       // focus the last prompt
sim.resize(40, 10);                 // the terminal is resized
sim.problems(&["MARK01"]);          // every row once, composer once, status once
println!("{}", sim.capture());      // scrollback, a rule, the screen, the cursor
```

The bottom block is a stand-in for the chat widget (a draft line and a status row); the
conversation cells and the focus view are real. For the real application, `world_ui`
(`docs/fake-worlds.md`) drives the `nori` binary under a pty.

## Input routing

`app/input/` routes by mode: [`global`] is one table of the keys that mean the same everywhere
(quit, clear screen, switch subagent, toggle tool); an open overlay owns what is left; otherwise
the event goes to the handler of its mode: focus (`FOCUS_KEYS` table), composer (`keys.rs`:
hotkeys, Esc), mouse (`mouse.rs`).
