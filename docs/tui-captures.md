# TUI captures

Nori's rendered terminal state can be captured from a live Rex pane and replayed
in a split for review. This preserves the terminal's current screen snapshot,
including colors, cursor position, and narrow or wide layouts; plain text is
provided only as a convenient search view. VT is a snapshot of the current
terminal screen and scrollback, rather than a time-based recording of every redraw.

The root conversation uses the terminal's native primary-screen scrollback: root
mouse reporting stays off so the terminal owns wheel scrolling, text selection,
and reflow. The conversation picker and focused child views temporarily capture
mouse input for their own wheel and click handling, then restore native root
scrollback when closed. Captures show the resulting rendered state; they do not
claim to replay mouse input.

From the repository root:

```sh
scripts/rex-capture capture tool-call --session "$REX_SESSION" --block "$REX_BLOCK" --freeze
```

The command writes `.nori-captures/tool-call/{screen.vt,screen.html,screen.txt,metadata.json}`
and opens a frozen split. Captures include the complete visible scrollback, so
agent messages, tool calls, approval views, and subagent switcher states are
recorded together. VT and HTML are requested through Rex's structured `format`
API with the live palette, style, and cursor included; mouse/keyboard modes are
not captured or replayed. Metadata includes the actual terminal rows, columns,
and cell pixel dimensions. Use `--split below --ratio 35` to compare a narrow
capture beside the live pane.

Replay a capture later with:

```sh
scripts/rex-capture replay .nori-captures/tool-call \
  --session "$REX_SESSION" --block "$REX_BLOCK"
```

For deterministic live states, launch the ACP wire fixtures in real Nori panes:

```sh
scripts/rex-scenario subagents --session "$REX_SESSION" --block "$REX_BLOCK" --split right --ratio 50
scripts/rex-scenario child --session "$REX_SESSION" --block "$REX_BLOCK" --split below --ratio 35
# Dedicated practice tabs (no working-pane split required)
scripts/rex-scenario child --session "$REX_SESSION" --tab
```

`subagents` renders Claude Agent updates; `child` renders an OpenCode child and
grandchild, including their tool calls and messages. Submit a prompt in the new
pane, wait for the fixture to settle, then capture that pane with
`scripts/rex-capture capture <name> --freeze`. The launcher prints the temporary
config path and keeps it alive for the pane's lifetime.

Use `--tab` for practice sessions: each scenario becomes a named Rex tab that
can be closed or refreshed independently. Replay captures in the same way with
`scripts/rex-capture replay <dir> --session "$REX_SESSION" --tab`.

For interaction proof, record a bounded sequence of real Rex frames:

```sh
scripts/rex-record --session "$REX_SESSION" --block "$REX_BLOCK" \
  --output .nori-captures/practice-recording --duration 12 --interval 0.5
```

The recorder writes timestamped VT and HTML frames, a manifest containing the
terminal geometry for every frame, and a standalone `player.html`. The player
uses an isolated iframe for each HTML frame and provides play/pause and a
scrubber. Playback follows each frame's recorded elapsed timestamp rather than
assuming the sampling interval. The iframe scrolls to the bottom on each frame
so the current composer/picker state is visible even when the capture includes
long scrollback; the full scrollback remains available inside the frame.
Durations are capped at 30 seconds and intervals at 5 seconds; no mouse or
keyboard modes are recorded or replayed.

HTML is useful for a browser or artifact viewer; VT is the canonical replay
format. The metadata records the Rex process and terminal modes at capture time.
It also records the captured text row/column envelope. Replay restores the
captured screen content but cannot resize a Rex pane; use a dedicated tab or a
matching split ratio when comparing narrow and wide states.
The three formats are captured sequentially, so a rapidly changing pane can
produce slightly different tails; repeat the capture after the state settles
when exact cross-format comparison matters.

Fixture tabs override real providers in their temporary configuration so background
warming cannot launch provider processes or startup hooks. Real-agent practice
chats use their own configuration and keep the user's existing authentication.
