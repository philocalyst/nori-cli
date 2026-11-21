# The editor

Every text the user edits in the TUI is one component, `nori_editor::Editor`
(`nori-rs/editor`): the draft in the composer, and every past prompt walked back to with `↑`
(see [tui-stage.md](tui-stage.md#walking-back-and-rewinding)), which is loaded into the same
editor. Same keys, same look, same behaviour everywhere.

## Engine

The editor is built on modalkit, vendored at `vendor/modalkit` (Apache-2.0; Nori's changes are
listed in `vendor/modalkit/NORI.md`). modalkit's `EditBuffer` holds the text and does the
editing: multiple selections, registers, an undo history with checkpoints, text objects, regex
search, macros, `.` repeat, counts. A keymap machine turns keys into editing actions; the editor
applies them and carries out the few the buffer cannot express itself:

- Helix's word motions (`w` `b` `e` `W` `B` `E`), which select end-exclusive
  (`nori-rs/editor/src/words.rs`, derived from Helix, MPL-2.0);
- surround editing (`ms` `mr` `md`);
- the clipboard (`space y` `space p` `space P` `space R`, read and written off the UI thread);
- the command palette (`:`), language-server jumps (`gd` `gy` `gr` `gi`, `space k`).

## Keymaps

`[tui] editor_mode` chooses the keymap:

| `editor_mode` | Keymap |
|---|---|
| `helix` (default; `kakoune` is accepted as a synonym) | Helix, selection first, modal |
| `vim` | Vim (modalkit's Vim environment); `vim_mode` sets what Enter does |
| `standard` | modeless: typing inserts, shifted motions select, `Ctrl+Z` undoes by word |

The modal keymaps start in Insert mode, so typing works at once, and return to it after a prompt
is sent. Insert mode in every keymap has the readline keys: `Ctrl+A` `Ctrl+E` (line start, end),
`Ctrl+B` `Ctrl+F`, `Alt+B` `Alt+F` (char, word), `Ctrl+W` `Alt+Backspace` (kill word back),
`Alt+D` (kill word forward), `Ctrl+U` `Ctrl+K` (kill to line start, end), `Ctrl+Y` (put back what
was killed), `Ctrl+J` (newline). The configurable editing hotkeys in `[tui.hotkeys]` are
translated to these keys while typing.

Helix's Normal mode, as in Helix: `h` `j` `k` `l` and the arrows; `w` `b` `e` `W` `B` `E`
select words; `f` `t` `F` `T` find characters; `x` `X` `%` select lines and everything; `v`
select mode (motions extend); `mi<obj>` `ma<obj>` select inside or around a text object
(`w` `W` `p` `s` `(` `[` `{` `<` `'` `"` `` ` `` `m`); `mm` matching bracket; `ms<c>` `mr<c><c>`
`md<c>` surround; `s` `S` `K` `Alt+K` select, split, keep and remove by regex; `Alt+S` split
lines; `C` `Alt+C` copy the selection to the next or previous line (multiple cursors); `,`
`Alt+,` `;` `Alt+;` `(` `)` manage selections; `d` `c` `y` `p` `P` `R` `r<c>` `~` `` ` ``
`Alt+`` ` `>` `<` `J` `Ctrl+A` `Ctrl+X` edit; `u` `U` undo and redo; `/` `?` `n` `N` `*` search;
`"<reg>` registers (uppercase appends); `Q` `q` record and replay a macro; `.` repeat;
`g` goto (`gg` `ge` `gh` `gl` `gs` `gj` `gk` `g.`), `z` view, `space` the space menu.

`Esc` belongs to the editor while it has a use for it (leaving Insert or Select mode, closing a
menu or the search prompt); only then do the composer and the app see it (ending a walk,
interrupting a turn, stopping subagents). An `Esc` within half a second of one that left Insert
mode never interrupts a running turn.

## Drawing

What the editor draws is a pure function of the text, its chips and the width
(`nori_editor::Layout`): lines wrap at word boundaries the way prose is wrapped elsewhere in the
TUI, whitespace hangs off the end of its row instead of starting the next one, and one column is
kept free so the cursor can sit after the last character of a full row. Typing, pasting and
moving never move text that is not being edited; a resize reflows everything at once. The only
drawing state is the first row shown, which moves just far enough to keep the cursor visible and
never leaves empty rows under the text while text is hidden above. Vertical moves (`↑` `↓`, and
`j` `k` in Helix) follow the drawn rows and keep their column.

Selections are drawn on the code-well surface, secondary cursors (and the primary one outside
Insert mode) as reversed cells; the terminal cursor is a bar in Insert mode and a block
elsewhere.

## Chips

A chip is an atomic token in the text, drawn as a label: a large paste
(`[Pasted Content 2000 chars]`), an attached image, and (with actions) references and
directives. In the buffer a chip is one character from Unicode's Supplementary Private Use
Area-A, so motions, deletion, yank and paste, undo and multiple selections treat it as one unit.
The editor keeps each chip's label and tone; what a chip stands for belongs to whoever inserted
it. The composer sends a paste chip as the pasted text and an image chip as its label with the
image attached.

## Latency

Every key is handled on the UI thread without IO: the buffer, the keymap and the layout are in
memory, the layout is cached per text revision and width, and the clipboard is read and written
on a thread of its own. Key-to-frame time is measured with the rest of the TUI's interactions
(`NORI_LATENCY_LOG`, see [responsiveness.md](responsiveness.md)).
