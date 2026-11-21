# modalkit, vendored for Nori

Source: https://github.com/ulyssa/modalkit at commit
`da2e8d33d73fdec407ab12451d8b9191a99bb2a1` (the `modalkit 0.0.32` release), Apache-2.0
(see `LICENSE`). Only the crates Nori builds are kept: `keybindings`, `editor-types`,
`editor-types-macros`, `editor-types-parser` and `modalkit`. `modalkit-ratatui` (Nori draws
its own editor, in `nori-rs/editor`), `scansion`, examples and upstream CI files are dropped.

The crates are wired into `nori-rs` through `[patch.crates-io]`, so every crate in the
workspace resolves `modalkit` and friends to this tree.

Nori's changes to upstream files are marked in the files themselves with a
`Modified for Nori` notice, as the Apache License asks; new files say they are Nori's:

- `crates/modalkit/src/env/helix/` (new): a Helix keymap environment (Normal, Select, Insert,
  goto/match/view/space menus, text objects, surround, registers, counts, macros, search),
  derived from the Kakoune environment. Motions Helix defines differently from modalkit's
  inclusive selections (word motions), surround edits, the space menu and language-server jumps
  are produced as `HelixCommand`s for the application to carry out. `a` steps past the
  selection's last character before typing (Kakoune's `cursor-set -f end` alone types before
  it).
- `crates/modalkit/src/env/mod.rs`: declares the Helix environment.
- `crates/modalkit/src/editing/buffer/mod.rs`: `EditBuffer::edit_text` (replace the text as an
  undoable edit, adjusting cursors) and `EditBuffer::cursor_group` (read every cursor).
