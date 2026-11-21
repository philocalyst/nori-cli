//! The agent-facing tool definitions: name, description and JSON schema. The descriptions are
//! written for Nori; they follow the onboarding pattern (what it does, when to reach for it, when
//! not to, examples).

use serde_json::Value;
use serde_json::json;

use super::ToolSpec;

fn position_properties() -> Value {
    json!({
        "path": {
            "type": "string",
            "description": "File containing the symbol, absolute or relative to the workspace."
        },
        "line": {
            "type": "integer",
            "minimum": 1,
            "description": "1-based line number where the symbol appears."
        },
        "symbol": {
            "type": "string",
            "description": "The symbol's name as written on that line (for example `parse_config`). Prefer this over `column`: it is looked up on the line, and on the nearest lines if the line number is slightly off."
        },
        "column": {
            "type": "integer",
            "minimum": 1,
            "description": "1-based column, counted in characters. Only needed when the same name appears several times on the line and `symbol` alone would be ambiguous."
        }
    })
}

fn with_position(extra: Value, required: &[&str]) -> Value {
    let mut properties = position_properties();
    if let (Some(base), Some(more)) = (properties.as_object_mut(), extra.as_object()) {
        base.extend(more.clone());
    }
    let mut needed = vec!["path", "line"];
    needed.extend_from_slice(required);
    json!({ "type": "object", "properties": properties, "required": needed })
}

pub(super) fn all() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "lsp_diagnostics",
            description: "Reports compiler and linter errors and warnings straight from the project's language server, without building.

Use it when:
- you have edited code and want to know whether it type-checks (edit tools already append a diagnostic summary for the edited file; call this for other files or to look again);
- you want an overview of what is currently broken in the project (call it with no path).

Do not use it to run tests, or for files whose language has no server (the answer says so; use the build command through the shell instead).

Examples:
- `{\"path\": \"src/server.rs\"}` lists the errors and warnings of that file, errors first, as `severity line:col [source code] message`.
- `{}` lists the files that currently have errors and the state of every running language server.

Servers start the first time a file of their type is touched. A server that is still indexing may report nothing yet, and the answer says when that is the case."
                .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File to check, absolute or relative to the workspace. Omit it for a project-wide summary."
                    }
                }
            }),
        },
        ToolSpec {
            name: "lsp_definition",
            description: "Jumps to where a symbol is defined, using the language server (it understands scopes, imports and types; text search does not).

Use it when you meet a function, type, variable or module and need its source: it returns the file, line, column and the defining line. Set `kind` to `type_definition` to find the type of a variable, or to `implementation` to find the implementations of a trait or interface method.

Do not use it to find every use of a symbol (use lsp_references), or to search for text that is not a symbol (use grep).

Examples:
- `{\"path\": \"src/app.ts\", \"line\": 42, \"symbol\": \"loadConfig\"}` returns the definition of `loadConfig` called on line 42.
- `{\"path\": \"src/app.ts\", \"line\": 42, \"symbol\": \"config\", \"kind\": \"type_definition\"}` returns where the type of `config` is declared.

Read the returned location with the file tools to see the whole definition."
                .to_owned(),
            input_schema: with_position(
                json!({
                    "kind": {
                        "type": "string",
                        "enum": ["definition", "type_definition", "implementation"],
                        "description": "What to look up. Defaults to `definition`."
                    }
                }),
                &[],
            ),
        },
        ToolSpec {
            name: "lsp_references",
            description: "Finds every use of a symbol across the project, using the language server. It is exact: it distinguishes a function from another function with the same name, and finds uses through imports and re-exports.

Use it before changing a function signature, deleting something, or judging how widely a symbol is used.

Do not use it for plain text (use grep), and do not use it to rename (use lsp_rename, which computes the edits for you).

Example: `{\"path\": \"src/db.rs\", \"line\": 17, \"symbol\": \"connect\"}` lists each file with the lines that reference `connect`.

Results are grouped by file and capped by `limit`; the answer says how many were left out."
                .to_owned(),
            input_schema: with_position(
                json!({
                    "include_declaration": {
                        "type": "boolean",
                        "description": "Also list the declaration itself. Defaults to true."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum number of references to print. Defaults to 50."
                    }
                }),
                &[],
            ),
        },
        ToolSpec {
            name: "lsp_hover",
            description: "Shows what the language server knows about a symbol: its type or signature and its documentation, as markdown.

Use it to learn a function's parameters and return type, the type of a variable, or a library item's docs without opening its source.

Do not use it to find where something is defined (use lsp_definition).

Example: `{\"path\": \"src/main.py\", \"line\": 8, \"symbol\": \"requests\"}`."
                .to_owned(),
            input_schema: with_position(json!({}), &[]),
        },
        ToolSpec {
            name: "lsp_symbols",
            description: "Lists symbols, either the outline of one file or a project-wide search by name.

- With `path` only: the file's outline (functions, types, methods, constants) with line numbers, nested as the server nests them. Use it to see the shape of a file before reading it.
- With `query`: symbols across the project whose names match, each with its file and line. Use it to find a function or type when you know its name but not its file. Pass `path` too (any source file of the language) so the right server is started.

Do not use it for text that is not a symbol name (use grep).

Examples:
- `{\"path\": \"src/lib.rs\"}` outlines `src/lib.rs`.
- `{\"query\": \"SessionStore\", \"path\": \"src/lib.rs\"}` finds `SessionStore` anywhere in the Rust project."
                .to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File to outline, or, with `query`, any file of the language to search in."
                    },
                    "query": {
                        "type": "string",
                        "description": "Symbol name or part of one. When given, the whole project is searched."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum number of symbols to print. Defaults to 100."
                    }
                }
            }),
        },
        ToolSpec {
            name: "lsp_rename",
            description: "Previews a semantic rename of a symbol across the project. It asks the language server for the exact edits (declaration, every use, imports, other files) and shows them; it changes nothing.

Use it to rename a function, type, variable, field or module safely. Afterwards apply the edits with the edit tools, file by file, then check the result with lsp_diagnostics. The preview lists each changed line before and after, so you can apply the same replacements.

Do not use it for text that only looks like the name (comments and strings are not renamed; use a text replace for those), and not to change a symbol's behavior.

Example: `{\"path\": \"src/api.ts\", \"line\": 12, \"symbol\": \"fetchUser\", \"new_name\": \"loadUser\"}`.

If the server refuses (the symbol cannot be renamed, or the code does not compile), the answer says why."
                .to_owned(),
            input_schema: with_position(
                json!({
                    "new_name": {
                        "type": "string",
                        "description": "The new name for the symbol."
                    }
                }),
                &["new_name"],
            ),
        },
        ToolSpec {
            name: "lsp_call_hierarchy",
            description: "Shows who calls a function (`incoming`) or which functions it calls (`outgoing`), using the language server.

Use it to understand the blast radius of a change (incoming) or to trace what a function depends on (outgoing). It follows real calls, including through traits and imports; grep cannot.

Do not use it for non-function symbols (use lsp_references).

Example: `{\"path\": \"src/pipeline.go\", \"line\": 30, \"symbol\": \"Run\", \"direction\": \"incoming\"}` lists every caller of `Run` with the call sites inside each caller.

One level is returned; call it again on an entry to go deeper."
                .to_owned(),
            input_schema: with_position(
                json!({
                    "direction": {
                        "type": "string",
                        "enum": ["incoming", "outgoing"],
                        "description": "`incoming` for callers, `outgoing` for callees."
                    }
                }),
                &["direction"],
            ),
        },
    ]
}
