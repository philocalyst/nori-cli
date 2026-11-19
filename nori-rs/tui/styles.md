# Styling: tokens, not colors

Every color on screen comes from `nori_tui_components::Theme` (`crate::style::component_theme()` in
`nori-tui` returns the live one). Components never name a color. They name a role: `theme.card`,
`theme.text_secondary`, `theme.accent_amber`, `theme.badge.rewind`. The theme is built once from what
the terminal tells us; this file says what each token is for and how it degrades.

## Two kinds of color

**Surfaces are relative to the user's own background.** The terminal's background is read once
(`terminal_palette`), and every surface is that color mixed a few percent toward white (dark
terminals) or black (light ones). Nori's rows then read as part of the same page as the shell's
output above them and the scrollback they scroll into, instead of as a foreign panel.

| Token | What sits on it |
|---|---|
| `card` | raised content: tool cards, subagent lanes |
| `code_well` | wells behind code, commands and diffs (a step darker than the page) |
| `prompt_surface` | the user's prompt rows in the transcript |
| `rewind_surface` | the prompt a rewind is focused on (the page tinted amber) |
| `statusline` | the one row under the composer |
| `menu_surface`, `selected`, `row`, ... | pickers and menus |

**Accents and syntax come from a fixed palette.** Mint, cyan, violet, amber and coral for meaning;
a syntax set (keyword, function, type, string, flag, variable, operator, comment) for code, shell
commands and diffs. There is one palette tuned for dark backgrounds and one for light ones; the
luminance of the terminal background picks between them.

## The foreground ladder

Five steps, strongest to weakest. A row uses at most three.

1. `text_strong`: the one thing a row is about (a file name, a prompt, a badge's subject).
2. `text` (the default foreground): prose.
3. `text_secondary`: tool summaries and metadata that still read as content.
4. `muted`: counters and context that should be findable, not noticed.
5. `faint`: structure: tree rails, separators, timestamps.

## Accents

Accents are small. They mark a state or an identity; they never wash a paragraph.

- `accent_mint`: success, additions, the running/active actor, BASH.
- `accent_cyan`: information, links to other things, ACTORS, the running-actor count.
- `accent_violet`: the user's side of the conversation, RESUME, the branch.
- `accent_amber`: attention and rewind: the focus bar, a context window nearly full, REWIND.
- `accent_coral`: failure, deletions, rollback markers.

Agent identity keeps its own tokens (`provider_*`) and appears only on explicitly typed cells.

## Diffs

`diff_add` / `diff_del` are the line backgrounds; `diff_add_word` / `diff_del_word` the stronger
backgrounds of the tokens that changed. Syntax colors stay on top, so a changed word is still
readable as code.

## The status line

One row, `statusline` surface. Left: the mode badge, then agent and model. Right: branch, cost,
context percent and the number of running actors, shed from the left when the row is narrow. No key
hints; the `?` overlay lists keys.

The badge is bold dark text on the mode's own color (`badge.insert`, `normal`, `rewind`, `bash`,
`resume`, `actors`): `INS`, `NOR`, `REWIND 2/3`, `BASH`, `RESUME`, `ACTORS`. Pickers draw the same
badge with `bottom_pane::status_badge`, so a mode looks the same wherever it is shown.

## When true color is not available

Surfaces are only set when the terminal reports both its background and 24-bit color. Without them
every surface is unset (the terminal's own background shows through), accents fall back to the ANSI
colors named in `Theme::default()` (`green` mint, `cyan`, `magenta` violet, `yellow` amber, `red`
coral), and the badge becomes bold reverse video. Nothing is lost but the surface steps: structure
is carried by the foreground ladder, rails and rules, never by a background alone.

## Writing styled text (ratatui)

- Prefer `Stylize` helpers (`"text".dim()`, `.bold()`, `.italic()`) for modifiers; use a token's
  `Style` (`Span::styled(text, theme.accent_cyan)`) for anything with a color.
- Basic spans: `"text".into()`. Chain for readability and stay on one line after rustfmt.
- Avoid `.white()` and `.black()`: the default foreground contrasts with the user's theme, a fixed
  one may not. Do not hard-code `Color::Rgb(...)` outside `tui-components/src/theme`.
- Headers are `bold`; markdown headers keep their `#` signs.
- Never draw structure with a background alone (rails `▌`, rules `─` and glyphs `●` `↶` `⎇` keep
  the layout when surfaces are unset).

## Wrapping

- Wrap plain strings with `textwrap::wrap`; wrap ratatui `Line`s with the helpers in
  `tui/src/wrapping.rs` (`word_wrap_lines`, `word_wrap_line`).
- Indent wrapped lines with `RtOptions`' `initial_indent` / `subsequent_indent`, and prefix lists of
  lines with `prefix_lines` from `line_utils`.
- Cells hand the stage lines; the stage wraps them to the screen (`stage/rows.rs`), cutting only
  what cannot wrap (a URL).

(There are rules to catch most of this in `clippy.toml`.)
