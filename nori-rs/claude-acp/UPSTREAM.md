# nori-claude-acp (vendored base)

Derived from https://github.com/soddygo/claude-code-acp-rs (MIT), via the maintained fork at
`4d217a8d09f64e149a81db08f887ae08300feccf` of `~/Projects/claude-code-acp-rs` (branch `robust`). It is
linked into Nori as a library and run in-process, over an in-memory pipe, so there is no adapter
process; only Claude Code's own CLI is started.

What Nori kept: the driver (`agent/`, `session/`), the message converter, the PreToolUse permission
hook and settings rules, and the `acp` SDK MCP server that replaces the CLI's Bash, Read, Write and
Edit. What it removed: the standalone binary and its CLI, OpenTelemetry, the TypeScript-parity tools
that were never routed (Glob, Grep, Task, Web*, Notebook*, ...), external MCP server management (the
CLI connects to the client's servers itself), and the `sacp` fork. Connection serving, request
routing, error shapes and usage reporting come from `nori-acp-kit`.

Local changes are in `git log -- nori-rs/claude-acp`.
