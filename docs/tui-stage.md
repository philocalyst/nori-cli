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

Nori **prints** a row of the conversation, hands it to the terminal for good, as soon as it can
no longer change: its cell, and every cell before it, has **settled** (nothing in it is running or
animating). A printed row is never drawn again. The terminal scrolls it into the scrollback,
re-wraps it on a resize and selects it like any program's output, so text the person is reading or
has selected never changes under them.

Below the printed rows is the **region**, the only part of the screen Nori redraws: the **live
tail** (the conversation from the first cell that has not settled, such as a tool still running)
and, under it, the **bottom block**. At rest the live tail is empty and the region is the bottom
block alone.

The bottom block is the composer and its status line, what the running turn shows above them
(a running tool, the rest of the answer still streaming, the lanes of running subagents, the
plan, queued messages) and, while one is open, a **view**: a popup, a picker, an approval, a
provider's sign-in program. What the turn shows is output like any other: it pushes the
conversation up, and rows that pass the top go to the scrollback. A view, and a decoration of the
conversation (a focused prompt, an expanded card), **reclaims** the printed rows still on screen
instead: the region grows over them, the view clips them from the top, and they are printed again
the moment it closes. Reclaiming tells the terminal's buffer of the last frame exactly what those
rows hold, so the rows a view leaves alone are not written. Rows a resize re-wrapped are the
terminal's alone and are never reclaimed; a view opened after a resize pushes them up instead.

Once the conversation has pushed the bottom block to the screen's last row it stays there: when the
block shrinks (a subagent finishes, the turn ends) the rows it frees stay empty above it, inside
the region, until the conversation grows into them, rather than the prompt floating up. Toasts
show at the right end of the status line, never over the conversation.

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

`tui/src/stage/` owns what the region shows, what is printed and what leaves the screen.

- The **tail** is every row of the conversation that is not in the scrollback yet, hard-wrapped
  and cached per width (`stage/tail.rs`). Its first rows are printed (on screen above the
  region); the rest is the live tail, drawn into the frame's cell grid like any widget.
- The **boundary** is the first row that is not in the scrollback: a cell index and how many of
  its rows are committed. The stage also counts how many rows from the boundary are printed.
  After every frame (`Stage::settle`), the rows of settled cells it drew at the top of the region
  are printed and the region starts below them.
- One way rows leave the screen (`stage/write.rs::scroll_rows_off`): line feeds on the last row,
  the one scroll every terminal keeps history for. Printed rows leave first, as they are. A row of
  the live tail that leaves is written at the top just before its line feed when the top does not
  show it (it was clipped by a view, it changed since it was drawn, a long history appears at
  once). Either way it reaches the scrollback once and in order, the boundary moves past it, and
  the rows that stay are moved by the terminal, never written again.
- While the conversation is *decorated* (a prompt is focused, a tool card is expanded) it is
  clipped at the top instead of scrolling, so a view the person opened never pushes rows away.
- Content flows from where the program started; the bottom block follows it until the
  conversation fills the screen, then stays at the bottom (see above).
- Only onboarding and other screens that are not the conversation ask for a block as tall as the
  screen: the tail is neither drawn, scrolled nor committed meanwhile, and the next ordinary frame
  draws it again.

Every frame is one synchronized update written with one `write`.

### Clear screen

`Ctrl+L` pushes everything on screen into the scrollback, whole (it scrolls; `ESC[2J` and
`ESC[3J` are never sent) and starts a clean region with the composer at the top.

### Resize

A resize must not cost the visible conversation, and must never leave a copy of it behind. The
terminal moves the screen by itself: a shorter screen pushes the top rows into the scrollback so
the cursor stays on screen, a taller one may pull rows back (iTerm2, kitty, Ghostty, WezTerm, tmux
do; xterm does not), and a new width re-wraps every row, the region's own included: a row of the
tail may become three. Where the cursor went and how the region's rows re-wrapped say exactly
where the region now begins.

1. The event stream, right after it is handed `Event::Resize` (when nothing is waiting on the
   input), asks the terminal where the cursor is (`CursorReport::query`, `ESC[6n`) and leaves the
   answer for the next frame (`ResizeSignal`). A reader that is already waiting would take the
   reply, which is why it is asked here and not from a frame. A terminal that leaves three
   questions in a row unanswered is not asked again; one slow answer in a burst of resizes is not
   enough.
2. Every row Nori draws is a line of its own: it ends with an explicit move or an erase, never by
   running past the last column. So a reflowing terminal wraps each one again at the new width,
   through its last glyph, or to the end of the row when the row ends on a colored background
   (the erase kept the color, and terminals keep colored cells). `tui/reflow.rs` computes from the
   cells of the last frame how many rows each region row above the cursor became. For that to
   hold, a row is never erased from its middle: a terminal remembers how far a row ever reached,
   and one erased only from column 20 keeps its old length, so on a narrower screen its blanks
   wrap onto a row of their own and the scrollback comes back double spaced. A changed row with
   room left at its end is erased whole and its cells written again (`custom_terminal.rs`
   `diff_buffers`); one that fills the width writes only its changed cells. xterm does not
   reflow: it cuts every row at the new width (`WidthChange::Cuts`, recognized by
   `XTERM_VERSION` with no multiplexer inside it), and then every row stays one row.
3. `Presenter::recover_from_resize` puts the region's first row at the cursor's row less those
   re-wrapped rows. The printed rows above it were re-wrapped by the terminal too, a glyph at a
   time; the stage keeps them as they were written (`Printed`), so it works out how many screen
   rows each became and which cells are still wholly on screen, counted up from the region.
   Those cells are taken back (`Stage::reown_after_resize`): the region grows up over them and
   the next frame lays them out again at the new width, words wrapped and the full width used,
   then prints them again. Cells cut by the top of the screen or pushed into the scrollback are
   the terminal's alone. Without an answer every printed row is the terminal's
   (`Stage::forget_printed`). Rows of the region the terminal pushed past the top are in the
   scrollback now: the live tail's rows that went whole are marked committed
   (`Stage::note_lost_rows`), and one cut in two is drawn again whole.
4. The rows from the first row of the region down are erased, one row at a time (never `ESC[J` at
   the top of the screen, which tmux reads as "clear the screen" and pushes everything into the
   scrollback, and never `2J`/`3J`), and the tail is drawn again at the new width from there, down
   to the composer. Rows above the region, and the scrollback, are not touched: the terminal
   reflows them itself.
5. A bottom block that sat on the screen's last row stays there (the stage keeps it anchored), so
   the prompt never drifts as a window is dragged: rows a taller screen adds are left empty right
   above it, where the conversation grows into them. A terminal that pulls no rows back from the
   scrollback on a taller screen (Terminal.app, xterm) is the exception: the cursor stays on its
   row, so the rows it added are known to be empty ones at the bottom, and the block comes up
   under the conversation instead (`Stage::unanchor_after_bare_grow`). Kept on the last row, it
   would leave those rows between the conversation and the block, and a later, shorter screen
   would push the conversation off the top to keep them
   (`a_terminal_that_never_pulls_rows_back_loses_nothing`). A block that had not reached the
   bottom stays where it was.
6. Without an answer, a region that began on the first row still does; otherwise the shift is
   assumed to be the push a shorter screen needs (or the pull a taller one allows), less the rows
   the region grew by, and nothing is marked committed.

A focused prompt, a picker, a popup that clips the tail or a pager is a transient view: it is
redrawn at the new width, and its rows are never marked committed. A full-screen view keeps the
stage's record of which cells the printed rows above it belong to, so a resize under it takes
them back, or hands them to the scrollback, like any other. Rows of such a view that a
shrinking screen pushes into the scrollback are there twice once they commit, and a row cut in two
by the push leaves its first part there; both stay in the scrollback, never on screen.

## Walking back and rewinding

There is no rewind mode. Walking back through the conversation happens in the composer, like a
shell's history, with the same editor as any draft (`bottom_pane/chat_composer/prompt_walk.rs`):

| Key | Does |
|---|---|
| `↑` on the composer's first row (or an empty composer) | load the previous prompt into the composer; the draft is put aside |
| `↓` on its last row | the next prompt; past the newest, the draft comes back |
| `Enter` | rewind there: restore the files the later turns changed, fork the conversation before the prompt, send the edited text |
| `Alt+Enter` | the same, keeping the files as they are |
| `Ctrl+F` | a subagent goes on from that turn's answer; the conversation stays as it is |
| `Esc`, `Ctrl+C` | leave the walk; the draft comes back |

Past the oldest prompt of the conversation, `↑` goes on into earlier sessions' prompts as plain
recall (sent as a new prompt, nothing rewound). While a prompt of the conversation is loaded, it
is edited where it is: the stage draws the prompt as it always looks, with the composer's editor
in place of its text and the cursor in it, and everything after it dimmed, which is what a rewind
from there rolls back (`app/focus.rs` follows the composer's `FocusChanged`; the rows come from
`ChatComposer::inline_walk`). There is no text box under the conversation meanwhile: the composer
keeps one line that says where the walk is (`↶ turn 3 of 5 · ⏎ rewind here · ⌥⏎ keep files · esc
back`) over the status line. The rows the walk changes are reclaimed from the printed ones while
they are on screen; what the terminal shows of them is known from the printed rows, so the
reclaimed rows are drawn over exactly. Enter while a turn runs stops the turn and rewinds as soon
as it has stopped.

Every rewind ends in `App::fork_conversation_at`: the conversation is cut before the prompt, the
agent starts a new session told what came before (within the context budget), and the prompt is
sent. `↑` then `Enter` on the newest prompt is a retry.

A child's journal is walked with keys, in the child's view: `[` `]` or `↑` `↓` move between its
prompts, `Enter`/`r` roll back through one, `f` forks a subagent from its response, `Esc` leaves.

While subagents run and the agent is idle, the first `Esc` on an empty composer says "Esc again
stops N running subagents" and a second one within two seconds stops them.

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
follow that binding; the composer's typing prefixes (`!`, `/`, `@`, `?`) and `↑` (walking back)
are fixed and listed so the `?` list is complete. Unknown ids and fixed entries are ignored.
`composer.palette` (`Alt+X`) opens the action palette over the draft ([actions.md](actions.md)).
The keys that edit text are the editor's, not the keymap's: see [editor.md](editor.md).

## StageSim

`stage/sim.rs` (test builds) runs the stage on an in-memory terminal: a `vt100` screen with
scrollback that moves rows on a resize like a cursor-following terminal. `behave` picks what
terminals differ on (`TerminalBehavior`): whether a taller screen pulls rows back from the
scrollback, whether the cursor query is answered, and whether a new width wraps every line again
(most terminals; the default) or cuts each row (xterm). Rewrapping joins the rows the simulated
terminal wrapped itself, so a narrower and then wider screen behaves as tmux does. Frames are presented by
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
