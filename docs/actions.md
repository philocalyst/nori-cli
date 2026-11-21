# Actions

An action is something the user can make Nori do: switch the model, set the effort, start a new
session, move the session to another machine, choose the editor's keys. Every action is defined
once, as a `nori_actions::Action` in a `nori_actions::Registry` (`nori-rs/actions`), and every
way of reaching it reads from there. The TUI's built-in actions and what each does are in
`nori-rs/tui/src/actions.rs` and `tui/src/chatwidget/actions.rs`.

## An action

| Field | Meaning |
|---|---|
| `id` | dotted and stable (`model.set`, `command.new`): what plugins and key bindings name |
| `title`, `summary`, `keywords` | what the palette shows and searches |
| `names` | what is typed after `/` (`/model`); the first is canonical |
| `param` | `None`, `Choice(source)` (models, efforts, modes, agents, machines, skills, files, sessions, or a fixed list) or `Text` |
| `effect` | `Now`, or `OnSend`: chosen with a draft open, it becomes a directive in the draft |
| `requires` | `idle` (waits for the running turn), `main` (not in a subagent's view), `local`, `cloud` |
| `keys` | its bindings, as shown |
| `source` | `BuiltIn`, `Agent` (commands an agent advertises), `Prompt` (custom prompts), `Plugin(name)` |

Whether an action can run is checked against the `Situation` (a turn running, a subagent's view,
a cloud session); the palette lists an action that cannot run dimmed, with the reason, after the
ones that can.

## Reaching an action

- **The palette**: `Alt+X` (`composer.palette` in `[tui.keymap]`), or `:` in Helix's Normal mode.
  It opens over the composer with its own query line; the draft is never touched. Typing filters
  by name, then fuzzily by title, names and keywords; `Enter` or `Tab` chooses. An action that
  takes a value lists the values (`/model`: the agent's models), or takes it from the query
  (`model opus`). `Esc` steps back from the values, then closes.
- **Typed commands**: a one-line draft `/name value` that names an action runs it
  (`/model opus`, `/effort high`). `/name` alone keeps the slash-command popup's behaviour.
- **Keys**: the dials' next and previous actions are the `[tui.hotkeys]` bindings.
- **Directives**: an `OnSend` action chosen in the palette while there is a draft is put into
  the draft as a chip (`⚙ model: Fake Sonnet 5`). Choosing the same action again replaces the
  chip. When the draft is sent, its directives are applied first, in order, and the chips are
  left out of the text; a directive deleted from the draft (it is one character to the editor)
  is not applied. Changing the model mid-prompt this way never disturbs what is being written.

## Dials

`model.set`, `effort.set` and `mode.set` take a value the agent offers (matched by value or
label, ignoring case, or the one label containing what was typed) and apply it the way the
dial's key does: shown at once, sent to the agent in the background, the newest choice winning
over one still in flight. Without a value, `model.set` opens the model picker and the others the
`/config` panel. `model.next`, `model.previous` and the effort and mode equivalents step the
dial.

## Plugins

The registry takes actions from any source: registering an id again replaces the action (a
plugin may override a built-in), and `Registry::remove_source` drops a source's actions (an
agent's commands when it changes, a plugin's when it unloads). The TUI carries out the built-in
actions; choosing an action it has no code for says so instead of doing nothing silently.
