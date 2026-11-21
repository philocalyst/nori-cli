# Agent file tools

`nori-tools` (`nori-rs/tools`) is the one implementation of the file tools Nori gives an agent:
`read`, `write`, `edit`, `multi_edit` and the freeform `apply_patch`. It is a library with no
dependency on the harness, ACP or provider crates, so every agent path can use it: Nori's host
tools (see [host-tools.md](host-tools.md)), the Claude adapter's MCP tools, and the in-process
Codex and OpenCode engine. Each host registers the catalog with its model API and forwards a
**tool call** to `Workspace::call`; the result carries the text for the model and, for a changed
file, the structured change a client shows as a diff.

The tools are agent-facing capabilities. They do not decide permissions: the host asks for
approval before it calls them, exactly as for any other **tool call**.

## The tool surface

| Tool | Aliases | Arguments | Purpose |
| --- | --- | --- | --- |
| `read` | `Read`, `read_file`, `view`, `cat` | `file_path`, `offset`, `limit` | Numbered lines, a directory listing, or an image |
| `write` | `Write`, `write_file`, `write_to_file`, `create_file` | `file_path`, `content` | Create a file or replace one whole |
| `edit` | `Edit`, `str_replace`, `edit_file`, `replace`, `search_replace` | `file_path`, `old_string`, `new_string`, `replace_all` | Replace exact text; empty `old_string` on a missing path creates it |
| `multi_edit` | `MultiEdit`, `multiedit` | `file_path`, `edits[]` | Several edits to one file, all or nothing |
| `apply_patch` | `patch`, `applypatch` | the patch text (`input`) | Multi-file add, update, delete and move in the Codex patch grammar |

`catalog(EditStyle)` returns the specs: `Replace` (`read`, `write`, `edit`, `multi_edit`), `Patch`
(`read`, `apply_patch`, for models trained on the patch format) or `Both`. A `ToolSpec` is data:
name, description, JSON input schema, aliases, and a format (`Json`, or `Freeform` with the Lark
grammar of `apply_patch`). Descriptions are onboarding-style: when to use the tool, when not to,
the exact contracts, and an example.

### Input repair

Models call these tools with other harnesses' spellings. `Workspace::call` accepts any alias or
`mcp__server__` prefixed name in any case, and repairs only what is unambiguous: `path`,
`filePath` and similar become `file_path`; `old_str`, `oldString`, `new_str` become `old_string`,
`new_string`; `replaceAll` becomes `replace_all`; `"10"` becomes `10`; a stringified `edits` array
is parsed; `start_line`/`end_line` become `offset`/`limit`; Anthropic's
`str_replace_based_edit_tool` is routed by its `command` (`view`, `str_replace`, `create`). A
canonical key always wins over an alias. A call that cannot be repaired returns an error naming
the required arguments.

## Read ledger

Every `read` records, per file, a hash of the content, the modification time and the line ranges
shown. `edit`, `multi_edit` and `write` on an existing file consult the **read ledger**:

- never read: refused with "You have not read ... call `read` first";
- content differs from what was read (hash comparison, so a touch or an identical rewrite does
  not count): refused with "changed on disk since you last read it ... read it again";
- otherwise allowed. A tool's own write updates the ledger, so a chain of edits needs one read.

An empty file and a file that does not yet exist need no read. `WorkspaceConfig` can switch the
gate off (`require_read_before_edit`) for an agent that reads through the shell, and can require
a read before `*** Update File` hunks (`patch_requires_read`, off by default because a patch's
context lines already prove the model knows the file). `ReadLedger::snapshot`/`restore` move the
ledger with a session to another machine. Parallel calls on the same path are serialised by a
per-path lock.

## Edit tiers

`old_string` is located on an LF view of the file, so CRLF files match plain newlines, and the
file's own endings (and BOM) are written back: untouched lines keep their ending, new lines take
the file's dominant one. Matching goes from strict to forgiving and the first tier with any match
decides; a looser tier is never used to break a tie of a stricter one:

1. **Exact**.
2. **Line endings**: exact once CRLF is normalised (reported only for multi-line matches in a
   CRLF file).
3. **Unicode**: NFC, curly quotes and apostrophes, dashes, ellipsis, non-breaking and other
   Unicode spaces, zero-width characters folded.
4. **Trailing whitespace**: whole lines equal ignoring trailing spaces and tabs.
5. **Indentation**: whole lines equal ignoring indentation. `new_string` is re-indented to the
   file: a common prefix added or removed, or spaces converted to tabs (or tabs to spaces). The
   result note says what happened, for example `matched ignoring indentation; re-indented
   (+4 spaces)`. If the indentation cannot be reconciled the text is inserted as written and the
   note says so.

Beyond the tiers: line-number prefixes copied from `read` output (`    12→`) are stripped from
`old_string`/`new_string` when every non-blank line has one.

- **Several matches** are refused with the line number and text of each, and how to
  disambiguate (add context, or `replace_all`). With `replace_all` every match is replaced in one
  pass (re-indented per match in tier 5).
- **No match** is refused with the closest lines of the file (three to five, numbered like
  `read`, with similarity), what differs line by line (including "only the indentation differs"),
  and a note when `new_string` is already in the file.
- **multi_edit** applies its edits in order in memory; each sees the earlier results; the first
  failure names its position and nothing is written.
- **Nothing changed** (for example the forgiving match normalised to identical text) writes
  nothing and says so.
- A file over 32 MiB, a binary file or a non-UTF-8 file is refused with what to do instead. Files
  over 8 MiB only get exact matching.

## `read`

Lines come back as a right-aligned number, `→`, and the text, 2000 lines by default (the cap).
Lines over 2000 characters are shortened, output over 60,000 characters stops with the offset to
continue from, CRLF is not shown, an empty file is reported, a directory lists its entries (`/`
suffix for directories), and png/jpg/gif/webp are attached as images. PDFs and other binary files
are refused with a pointer to shell tools. A missing path returns `File not found` plus up to
three suggestions: other extensions of the same stem, names containing it, close spellings, or
the closest spelling of a missing directory component.

## `apply_patch`

The Codex grammar (`*** Begin Patch` ... `*** End Patch`; Add, Delete, Update with optional Move;
`@@` context lines, stackable; `*** End of File`). The whole patch is validated in memory first
and written only if every hunk applies; a failed write rolls the earlier writes back. Hunks are
located with the edit tiers 1, 4 and 5 plus Unicode folding, and re-indent their added lines.
`*** Add File` refuses to overwrite an existing file.

## Results and errors

`ToolResult` is `{ text, is_error, changes, summary, images }`. `changes` holds, per file, the
kind (created, modified, deleted), old and new text, a unified diff and line counts, which is what
a host needs for an ACP diff or an undo journal. `summary` has lines added and removed, the
replacement count, the loosest tier used and the notes. `structured()` gives a JSON form without
the file bodies. Every error is a typed `ToolError` whose text says how to recover (read again,
add context, use another tool); `Workspace::call` turns it into a result with `is_error` set and
never panics.

## Cancellation

`Workspace::call_with_cancel(name, args, token)` is `call` that stops promptly when a
`CancellationToken` fires (`call` is the same with a token that never fires). A call cancelled
before it starts writing returns an error result whose text is `cancelled by the user` and has
touched nothing: its reads are simply dropped, and no temporary file exists yet. A call is
uncancellable from its first write to its last: writes are atomic (temporary file and rename)
and the ledger is updated with them, so if the token fires inside that window the call finishes
and returns its real result. The race is decided by one compare-and-swap, so a file is never left
half written or out of step with the ledger. A token that is already cancelled returns at once.

## Previews for permission dialogs

`Workspace::preview(name, args) -> Result<Preview, ToolError>` is a dry run for the permission
layer. It runs the same input repair, the same match tiers and the same ledger checks as the real
call, so a call that would fail fails the same way (the same `ToolError`, with the same closest-lines
hint). It writes nothing and does not touch the ledger.

- `Preview.action` is `PermissionAction::Read` for `read` and `Edit` for everything else.
- `Preview.paths` lists every absolute path the call touches (for `apply_patch`, including moves
  and deletions), each with `external`, see below.
- `Preview.changes` is, per file the call would change, the kind (created, modified, deleted),
  `old_text` (`None` for a new file) and the exact `new_text`, with the file's own line endings.
  It is empty for a read and for an edit that would change nothing.

A preview is a snapshot: the file may change before the real call runs, and the real call checks
again.

## External directories

`WorkspaceConfig::roots` holds extra directories that count as inside the workspace, besides the
working directory. `Workspace::is_external(path)` is true for a path under none of them (lexical:
`..` is resolved, symlinks are not; a relative path is taken against the working directory), and
`Preview.paths[].external` carries the answer. The tools never refuse an external path: whether it
needs approval is the permission layer's decision.

## Filesystem seam

Everything goes through the async `ToolFs` trait (`stat`, `read`, `write`, `remove_file`,
`read_dir`). `LocalFs` is this machine: atomic writes through a temporary file and rename,
existing permissions kept, symlinks written through. Running the tools on another machine means
implementing `ToolFs` over that machine's transport and building the `Workspace` with it.
CPU-bound work (hashing, matching, diffing) over large files runs on the blocking pool so a call
never stalls the runtime.

## Attribution

Tool descriptions and the patch grammar adapt OpenCode (MIT), Codex and Gemini CLI (Apache-2.0).
`nori-rs/tools/NOTICE` lists every reused text and algorithm. Zed (GPL-3.0) and Crush (FSL-1.1)
were behavioural references only; none of their text or code is used.

## Where the tools are wired

- **Nori host tools and the `nori-client` MCP server** (`harness/src/backend/nori_client_mcp/file_tools.rs`):
  opt-in via workflow `tools.enabled`; edits append language server diagnostics. See
  [host-tools.md](host-tools.md).
- **Claude adapter** (`claude-acp/src/mcp/tools/{read,edit,write}.rs`): the `mcp__acp__Read`,
  `Write` and `Edit` tools keep their names and run `Workspace::call` on one workspace per
  session; the descriptions and schemas are the catalog's. The first change of a result becomes
  the ACP diff (`FileDiff`) for the edit journal. Image reads and `MultiEdit` are not exposed
  there yet.
