//! Model-facing tool descriptions.
//!
//! Adapted, with attribution (see the crate NOTICE), from OpenCode (MIT; the edit, read and write
//! contracts), OpenAI Codex (Apache-2.0; the `apply_patch` envelope and grammar), and Gemini CLI
//! (Apache-2.0; the "exact literal text, no placeholders" rule and the surgical-edit guidance).
//! Zed's and Crush's descriptions were read as behavioural references only; none of their text is
//! used.

pub(super) const READ: &str = "\
Reads a file or lists a directory on the local filesystem. Assume any path the user gives you is valid and read it; do not ask first.

Usage:
- `file_path` is absolute, or relative to the working directory.
- By default this returns up to 2000 lines from the start of the file. For a larger file, pass `offset` (the 1-based line to start at) and `limit`. Prefer one larger read over many small slices, and use `grep` to find the part of a large file you need.
- Every line comes back as its line number right-aligned in six columns, then `→`, then the text, for example `    12→  let x = 1;`. The number and arrow are display only and are NOT part of the file. When you copy text from a read result into `edit`, `multi_edit` or `write`, drop the prefix and keep exactly the indentation that follows the arrow.
- Lines longer than 2000 characters are shortened; the result says so.
- A directory returns its entries one per line, directories with a trailing `/`.
- Images (png, jpg, gif, webp) are attached for you to look at. PDFs and other binary files cannot be read here; use a shell command for those.
- Read several files in parallel when you need more than one: issue the calls in the same turn.
- You must read an existing file before you `edit`, `multi_edit` or `write` it. Those tools refuse a file you have not read in this session, or that changed on disk since you read it (a formatter, the user, another tool). If that happens, read it again and redo the change.
- If the path does not exist, the error lists similarly named files nearby.";

pub(super) const WRITE: &str = "\
Writes a file to the local filesystem, creating it or replacing it whole.

When to use:
- Creating a new file. Missing parent directories are created for you.
- Replacing an existing file completely. For a partial change use `edit` or `multi_edit`: they send only the changed text and leave the rest of the file untouched.

Rules:
- Overwriting an existing file requires that you `read` it first in this session; otherwise the call is refused. Creating a file that does not exist needs no read.
- `content` is the complete file, written literally. Never use placeholders such as `// ... rest of file ...`: they would be written into the file and the real code would be lost.
- If the file uses CRLF line endings and your content uses plain newlines, the file's CRLF endings are kept.
- Do not create documentation files (README, *.md) unless the user asked for them.
- If the new content equals what is already there, nothing is written and the result says so.";

pub(super) const EDIT: &str = "\
Replaces exact text in a file, or creates a new file. This is the tool for targeted changes to existing files: it sends only the text that changes. Use `multi_edit` for several changes to the same file, `write` for a whole new file, and `apply_patch` (if available) for changes across several files.

Before you edit:
- `read` the file first. An edit to a file you have not read, or that changed since you read it, is refused. After your own edits the file counts as read, so you can keep editing without reading again.
- `read` output starts every line with a line number and `→` (for example `    12→  let x = 1;`). That prefix is not in the file. Never put it in `old_string` or `new_string`, and keep the indentation (tabs or spaces) that follows the arrow exactly.

How it matches:
- `old_string` is the exact text to find: same characters, same whitespace, same indentation. Write line breaks as plain newlines; if the file uses CRLF the match works and the file's line endings are kept.
- By default `old_string` must identify exactly ONE place. If it matches more than once the edit is refused and the result lists the line number of every match. Add surrounding lines (a few lines before and after) until it is unique, or set `replace_all` to true to change every occurrence, for example when renaming a variable.
- `new_string` is the replacement and must differ from `old_string`. It is written literally: do not use placeholders such as `// ... unchanged ...`.
- Prefer the smallest `old_string` that is unique; include enough context to be sure it is the right spot, but do not repeat whole functions you are not changing.
- If there is no exact match, the tool retries while ignoring line endings, typographic quotes and dashes and non-breaking spaces, trailing whitespace, and finally indentation (your `new_string` is then re-indented to the file's indentation). The result tells you when it had to do this; check the diff. Do not rely on it: copy the text exactly.
- If nothing matches, the error shows the lines of the file closest to your `old_string` with line numbers and what differs. Use that to correct `old_string` rather than guessing again; if the file may have changed, `read` it again.

Creating a file: pass a `file_path` that does not exist and an empty `old_string`; `new_string` becomes the file's content. (`write` does the same.)

Example: change a return value.
  file_path: \"src/lib.rs\"
  old_string: \"    pub fn answer() -> i32 {\\n        41\\n    }\"
  new_string: \"    pub fn answer() -> i32 {\\n        42\\n    }\"";

pub(super) const MULTI_EDIT: &str = "\
Applies several find-and-replace edits to ONE file in a single call. Prefer it to repeated `edit` calls when you are changing several places in the same file.

- The edits are applied in order, in memory. Each edit sees the result of the earlier ones, so a later `old_string` may need to match text an earlier edit produced.
- It is all or nothing. If any edit fails, nothing is written and the error names the failing edit and why. Fix that edit and send the whole list again.
- Every edit follows the same rules as `edit`: `read` the file first; `old_string` must match exactly once unless that edit sets `replace_all`; `new_string` must differ from `old_string`; never include `read`'s line-number prefixes; copy indentation exactly.
- Make each `old_string` unique on its own, and make sure the edits do not overlap.
- To create a new file, make the first edit's `old_string` empty and use a path that does not exist.

Example:
  file_path: \"src/main.rs\"
  edits: [
    {\"old_string\": \"let timeout = 30;\", \"new_string\": \"let timeout = 60;\"},
    {\"old_string\": \"retry(3)\", \"new_string\": \"retry(5)\", \"replace_all\": true}
  ]";

pub(super) const APPLY_PATCH: &str = "\
Edits files with a patch. This is a FREEFORM tool: send the patch text itself, not JSON and not a shell command. Use it for changes that touch several files, or that add, delete or move files. For one or two changes in a single file, `edit` is simpler.

A patch is an envelope of file operations:

*** Begin Patch
[ one or more file operations ]
*** End Patch

Each operation starts with a header:
*** Add File: <path>      create a new file; every following line starts with `+`
*** Delete File: <path>   remove an existing file; nothing follows
*** Update File: <path>   change a file in place; may be followed by `*** Move to: <new path>` to rename it

An update contains one or more hunks. A hunk starts with `@@` (optionally followed by a line such as a class or function signature that locates it) and is made of lines that start with:
` ` (a space)  an unchanged context line
`-`            a line to remove
`+`            a line to add

For each change show 3 lines of context above and 3 below. If a change is within 3 lines of the previous one, do not repeat the shared context. When 3 lines do not identify a unique place, add an `@@` line naming the enclosing class or function; you may stack several `@@` lines (`@@ class Foo` then `@@ fn bar`) to narrow it down. `*** End of File` after a hunk anchors it at the end of the file.

Example:

*** Begin Patch
*** Add File: hello.txt
+Hello world
*** Update File: src/app.py
*** Move to: src/main.py
@@ def greet():
-print(\"Hi\")
+print(\"Hello, world!\")
*** Delete File: obsolete.txt
*** End Patch

Rules:
- Paths are relative to the working directory, never absolute.
- You must include a header for every operation, and prefix new lines with `+` even in a new file.
- Context and `-` lines must match the file. Indentation differences are tolerated and your added lines are re-indented to fit, but copy lines exactly from a `read`.
- The patch is applied all or nothing: if any hunk does not match, no file is changed and the error shows the closest lines of that file.";

/// The Lark grammar of the freeform `apply_patch` tool, from OpenAI Codex (Apache-2.0).
pub(super) const APPLY_PATCH_GRAMMAR: &str = "\
start: begin_patch hunk+ end_patch
begin_patch: \"*** Begin Patch\" LF
end_patch: \"*** End Patch\" LF?

hunk: add_hunk | delete_hunk | update_hunk
add_hunk: \"*** Add File: \" filename LF add_line+
delete_hunk: \"*** Delete File: \" filename LF
update_hunk: \"*** Update File: \" filename LF change_move? change?

filename: /(.+)/
add_line: \"+\" /(.*)/ LF -> line

change_move: \"*** Move to: \" filename LF
change: (change_context | change_line)+ eof_line?
change_context: (\"@@\" | \"@@ \" /(.+)/) LF
change_line: (\"+\" | \"-\" | \" \") /(.*)/ LF
eof_line: \"*** End of File\" LF

%import common.LF
";
