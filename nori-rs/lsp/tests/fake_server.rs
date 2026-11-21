//! A tiny in-process language server speaking real LSP JSON-RPC over a pipe, and the manager
//! driven against it. No real language server is involved.
//!
//! The fake understands just enough: it publishes an error for every line containing `ERROR`
//! (and one for `b.rs` when the text contains `BREAK_B`), answers definition, hover, references,
//! rename and document symbols by looking up `fn <name>` lines, and exits when it sees `CRASH`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicI32;
use std::sync::atomic::Ordering;
use std::time::Duration;

use helix_lsp::Client;
use nori_lsp::CallDirection;
use nori_lsp::Diagnostic;
use nori_lsp::LaunchRequest;
use nori_lsp::Launched;
use nori_lsp::LspConfig;
use nori_lsp::LspError;
use nori_lsp::LspManager;
use nori_lsp::RenderOptions;
use nori_lsp::ReportStatus;
use nori_lsp::ServerConfig;
use nori_lsp::ServerLauncher;
use nori_lsp::Severity;
use nori_lsp::Target;
use nori_lsp::Timing;
use nori_lsp::render_summary;
use nori_lsp::tools::TOOL_NAMES;
use nori_lsp::tools::{self};
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::io::DuplexStream;

#[derive(Default)]
struct Probe {
    launches: AtomicI32,
    exits: AtomicI32,
    answered_configuration: AtomicBool,
}

struct FakeLauncher {
    probe: Arc<Probe>,
    available: bool,
}

impl ServerLauncher for FakeLauncher {
    fn resolve(&self, server: &ServerConfig) -> Option<PathBuf> {
        (self.available && server.name == "rust-analyzer")
            .then(|| PathBuf::from("/fake/rust-analyzer"))
    }

    fn launch(&self, request: LaunchRequest) -> Result<Launched, LspError> {
        self.probe.launches.fetch_add(1, Ordering::SeqCst);
        let (client_end, server_end) = tokio::io::duplex(1 << 20);
        let (server_read, server_write) = tokio::io::split(server_end);
        let (client_read, client_write) = tokio::io::split(client_end);
        tokio::spawn(fake_server(
            BufReader::new(server_read),
            server_write,
            Arc::clone(&self.probe),
        ));
        let root_uri = helix_lsp::lsp::Url::from_directory_path(&request.root).ok();
        let (client, incoming, initialize_notify) = Client::start_with_io(
            BufReader::new(client_read),
            client_write,
            BufReader::new(tokio::io::empty()),
            None,
            request.root,
            root_uri,
            request.id,
            request.server.name,
            5,
        );
        Ok(Launched {
            client,
            incoming,
            initialize_notify,
        })
    }
}

async fn read_message(reader: &mut BufReader<tokio::io::ReadHalf<DuplexStream>>) -> Option<Value> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await.ok()? == 0 {
            return None;
        }
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length: ") {
            length = value.parse::<usize>().ok();
        }
    }
    let mut body = vec![0; length?];
    reader.read_exact(&mut body).await.ok()?;
    serde_json::from_slice(&body).ok()
}

async fn send(writer: &mut tokio::io::WriteHalf<DuplexStream>, message: Value) {
    let body = message.to_string();
    let framed = format!("Content-Length: {}\r\n\r\n{body}", body.len());
    let _ = writer.write_all(framed.as_bytes()).await;
    let _ = writer.flush().await;
}

fn word_at(line: &str, character: usize) -> String {
    let chars: Vec<char> = line.chars().collect();
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut start = character.min(chars.len());
    let mut end = start;
    while start > 0 && is_word(chars[start - 1]) {
        start -= 1;
    }
    while end < chars.len() && is_word(chars[end]) {
        end += 1;
    }
    chars[start..end].iter().collect()
}

fn range(line: usize, start: usize, end: usize) -> Value {
    json!({
        "start": { "line": line, "character": start },
        "end": { "line": line, "character": end }
    })
}

fn occurrences(text: &str, word: &str) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let mut from = 0;
        while let Some(at) = line[from..].find(word) {
            let start = from + at;
            let before_ok =
                start == 0 || !line[..start].ends_with(|c: char| c.is_alphanumeric() || c == '_');
            let after = &line[start + word.len()..];
            let after_ok = !after.starts_with(|c: char| c.is_alphanumeric() || c == '_');
            if before_ok && after_ok {
                found.push((index, start));
            }
            from = start + word.len();
        }
    }
    found
}

fn definition_of(text: &str, word: &str) -> Option<(usize, usize)> {
    text.lines().enumerate().find_map(|(index, line)| {
        let rest = line.strip_prefix("fn ")?;
        rest.starts_with(word).then_some((index, 3))
    })
}

fn diagnostics_for(text: &str) -> Vec<Value> {
    text.lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let at = line.find("ERROR")?;
            Some(json!({
                "range": range(index, at, at + 5),
                "severity": 1,
                "source": "fake",
                "code": "E1",
                "message": "fake error"
            }))
        })
        .collect()
}

async fn fake_server(
    mut reader: BufReader<tokio::io::ReadHalf<DuplexStream>>,
    mut writer: tokio::io::WriteHalf<DuplexStream>,
    probe: Arc<Probe>,
) {
    let mut docs: HashMap<String, String> = HashMap::new();
    while let Some(message) = read_message(&mut reader).await {
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let id = message.get("id").cloned();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let uri = params["textDocument"]["uri"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        match method {
            "" => {
                // A response to a request we made.
                if id == Some(json!(900)) {
                    probe.answered_configuration.store(true, Ordering::SeqCst);
                }
            }
            "initialize" => {
                let capabilities = json!({
                    "positionEncoding": "utf-16",
                    "textDocumentSync": { "openClose": true, "change": 1, "save": {} },
                    "definitionProvider": true,
                    "hoverProvider": true,
                    "referencesProvider": true,
                    "renameProvider": true,
                    "documentSymbolProvider": true,
                    "workspaceSymbolProvider": true,
                    "callHierarchyProvider": true
                });
                send(&mut writer, json!({ "jsonrpc": "2.0", "id": id, "result": { "capabilities": capabilities } })).await;
            }
            "initialized" => {
                send(
                    &mut writer,
                    json!({
                        "jsonrpc": "2.0", "id": 900, "method": "workspace/configuration",
                        "params": { "items": [{ "section": "fake" }] }
                    }),
                )
                .await;
            }
            "textDocument/didOpen" | "textDocument/didChange" => {
                let (text, version) = if method == "textDocument/didOpen" {
                    (
                        params["textDocument"]["text"]
                            .as_str()
                            .unwrap_or("")
                            .to_owned(),
                        params["textDocument"]["version"].clone(),
                    )
                } else {
                    (
                        params["contentChanges"][0]["text"]
                            .as_str()
                            .unwrap_or("")
                            .to_owned(),
                        params["textDocument"]["version"].clone(),
                    )
                };
                if method == "textDocument/didChange" && text.contains("CRASH") {
                    probe.exits.fetch_add(1, Ordering::SeqCst);
                    return;
                }
                docs.insert(uri.clone(), text.clone());
                tokio::time::sleep(Duration::from_millis(30)).await;
                send(&mut writer, json!({
                    "jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
                    "params": { "uri": uri, "version": version, "diagnostics": diagnostics_for(&text) }
                })).await;
                if text.contains("BREAK_B")
                    && let Some(b_uri) = docs.keys().find(|key| key.ends_with("/b.rs")).cloned()
                {
                    send(
                        &mut writer,
                        json!({
                            "jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
                            "params": { "uri": b_uri, "diagnostics": [{
                                "range": range(0, 0, 2), "severity": 1, "message": "b is broken now"
                            }] }
                        }),
                    )
                    .await;
                }
            }
            "textDocument/definition"
            | "textDocument/hover"
            | "textDocument/references"
            | "textDocument/rename" => {
                let text = docs.get(&uri).cloned().unwrap_or_default();
                let position = &params["position"];
                let line = position["line"].as_u64().unwrap_or(0) as usize;
                let character = position["character"].as_u64().unwrap_or(0) as usize;
                let word = word_at(text.lines().nth(line).unwrap_or(""), character);
                let result = match method {
                    "textDocument/definition" => match definition_of(&text, &word) {
                        Some((l, c)) => json!({ "uri": uri, "range": range(l, c, c + word.len()) }),
                        None => Value::Null,
                    },
                    "textDocument/hover" => json!({
                        "contents": { "kind": "markdown", "value": format!("```rust\nfn {word}()\n```\nDocs for {word}") }
                    }),
                    "textDocument/references" => Value::Array(
                        occurrences(&text, &word)
                            .into_iter()
                            .map(|(l, c)| json!({ "uri": uri, "range": range(l, c, c + word.len()) }))
                            .collect(),
                    ),
                    _ => {
                        let new_name = params["newName"].as_str().unwrap_or("");
                        let edits: Vec<Value> = occurrences(&text, &word)
                            .into_iter()
                            .map(|(l, c)| json!({ "range": range(l, c, c + word.len()), "newText": new_name }))
                            .collect();
                        json!({ "changes": { uri.clone(): edits } })
                    }
                };
                send(
                    &mut writer,
                    json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                )
                .await;
            }
            "textDocument/documentSymbol" => {
                let text = docs.get(&uri).cloned().unwrap_or_default();
                let symbols: Vec<Value> = text
                    .lines()
                    .enumerate()
                    .filter_map(|(index, line)| {
                        let name = line.strip_prefix("fn ")?.split('(').next()?.to_owned();
                        Some(json!({
                            "name": name, "kind": 12, "detail": "()",
                            "range": range(index, 0, line.len()),
                            "selectionRange": range(index, 3, 3 + name.len())
                        }))
                    })
                    .collect();
                send(
                    &mut writer,
                    json!({ "jsonrpc": "2.0", "id": id, "result": symbols }),
                )
                .await;
            }
            "workspace/symbol" => {
                let query = params["query"].as_str().unwrap_or("");
                let mut found = Vec::new();
                for (doc_uri, text) in &docs {
                    for (index, line) in text.lines().enumerate() {
                        if let Some(name) = line
                            .strip_prefix("fn ")
                            .and_then(|rest| rest.split('(').next())
                            && name.contains(query)
                        {
                            found.push(json!({
                                "name": name, "kind": 12,
                                "location": { "uri": doc_uri, "range": range(index, 3, 3 + name.len()) }
                            }));
                        }
                    }
                }
                send(
                    &mut writer,
                    json!({ "jsonrpc": "2.0", "id": id, "result": found }),
                )
                .await;
            }
            "shutdown" => {
                send(
                    &mut writer,
                    json!({ "jsonrpc": "2.0", "id": id, "result": null }),
                )
                .await;
            }
            "exit" => {
                probe.exits.fetch_add(1, Ordering::SeqCst);
                return;
            }
            _ => {
                if let Some(id) = id {
                    send(
                        &mut writer,
                        json!({ "jsonrpc": "2.0", "id": id, "result": null }),
                    )
                    .await;
                }
            }
        }
    }
}

fn timing() -> Timing {
    Timing {
        first_diagnostics: Duration::from_millis(800),
        settle: Duration::from_millis(80),
        settle_cap: Duration::from_secs(2),
        init: Duration::from_secs(5),
        request: Duration::from_secs(5),
        ready_wait: Duration::from_secs(5),
        shutdown: Duration::from_secs(1),
        backoff_base: Duration::from_millis(20),
        backoff_cap: Duration::from_millis(50),
    }
}

struct Project {
    dir: tempfile::TempDir,
    manager: LspManager,
    probe: Arc<Probe>,
}

impl Project {
    fn file(&self, name: &str) -> PathBuf {
        self.dir.path().canonicalize().unwrap().join(name)
    }

    fn write(&self, name: &str, text: &str) -> PathBuf {
        let path = self.file(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    }
}

fn project_with(available: bool, max_restarts: i32) -> Project {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"p\"\n").unwrap();
    let probe = Arc::new(Probe::default());
    let launcher = Arc::new(FakeLauncher {
        probe: Arc::clone(&probe),
        available,
    });
    let config = LspConfig {
        timing: timing(),
        max_restarts,
        ..LspConfig::default()
    };
    let manager = LspManager::with_launcher(dir.path(), config, launcher);
    Project {
        dir,
        manager,
        probe,
    }
}

fn project() -> Project {
    project_with(true, 3)
}

#[tokio::test]
async fn diagnostics_after_edit_waits_for_the_server_and_renders_a_summary() {
    let p = project();
    let a = p.write("src/a.rs", "");
    p.manager
        .notify_file_changed(&a, "fn main() {\n ERROR here\n}\n".to_owned())
        .await
        .unwrap();
    let report = p.manager.diagnostics_after_edit(&a).await;
    assert_eq!(report.status, ReportStatus::Fresh);
    assert_eq!(report.servers, vec!["rust-analyzer".to_owned()]);
    assert_eq!(report.file.len(), 1);
    let d: &Diagnostic = &report.file[0];
    assert_eq!(
        (d.severity, d.range.line, d.range.column),
        (Severity::Error, 2, 2)
    );
    assert_eq!(d.source.as_deref(), Some("fake"));
    assert_eq!(d.code.as_deref(), Some("E1"));

    let text = render_summary(&report, &RenderOptions::new(p.manager.workspace_root()));
    assert_eq!(
        text,
        "<diagnostic_summary>\nsrc/a.rs: 1 error, 0 warnings\n  error 2:2 [fake E1] fake error\n</diagnostic_summary>"
    );

    // Fixing the file clears it: the server publishes an empty list.
    p.manager
        .notify_file_changed(&a, "fn main() {}\n".to_owned())
        .await
        .unwrap();
    let report = p.manager.diagnostics_after_edit(&a).await;
    assert_eq!(report.status, ReportStatus::Fresh);
    assert_eq!(report.file, Vec::new());
    assert!(p.probe.answered_configuration.load(Ordering::SeqCst));
}

#[tokio::test]
async fn errors_an_edit_causes_in_other_files_are_reported_separately() {
    let p = project();
    let a = p.write("a.rs", "");
    let b = p.write("b.rs", "");
    p.manager
        .notify_file_changed(&b, "fn b() {}\n".to_owned())
        .await
        .unwrap();
    p.manager.diagnostics_after_edit(&b).await;

    p.manager
        .notify_file_changed(&a, "fn a() {}\n// BREAK_B\n".to_owned())
        .await
        .unwrap();
    let report = p.manager.diagnostics_after_edit(&a).await;
    assert_eq!(report.file, Vec::new());
    assert_eq!(report.project.len(), 1);
    assert_eq!(report.project[0].path, b);
    assert_eq!(report.project[0].diagnostics[0].message, "b is broken now");
    let text = render_summary(&report, &RenderOptions::new(p.manager.workspace_root()));
    assert!(text.contains("New errors in other files:\nb.rs: 1 error"));
}

#[tokio::test]
async fn diagnostics_subscribers_are_woken() {
    let p = project();
    let a = p.write("a.rs", "");
    let mut rx = p.manager.subscribe_diagnostics();
    p.manager
        .notify_file_changed(&a, "ERROR\n".to_owned())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), rx.changed())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(p.manager.diagnostics(&a).len(), 1);
}

#[tokio::test]
async fn definition_hover_references_and_symbols() {
    let p = project();
    let a = p.write(
        "a.rs",
        "fn foo() {}\nfn main() {\n    foo();\n    foo();\n}\n",
    );

    // The symbol is found on the line even though the column is wrong.
    let target = Target::new(&a, 3).column(1).symbol("foo");
    let definition = p.manager.definition(&target).await.unwrap();
    assert_eq!(definition.target.column, 5);
    assert_eq!(definition.value.len(), 1);
    assert_eq!(definition.value[0].path, a);
    assert_eq!(
        (
            definition.value[0].range.line,
            definition.value[0].range.column
        ),
        (1, 4)
    );
    assert_eq!(definition.value[0].snippet, "fn foo() {}");

    let hover = p.manager.hover(&target).await.unwrap().value.unwrap();
    assert!(hover.text.contains("Docs for foo"));

    let references = p.manager.references(&target, true).await.unwrap();
    assert_eq!(references.value.len(), 3);

    let symbols = p.manager.document_symbols(&a).await.unwrap();
    assert_eq!(
        symbols
            .iter()
            .map(|s| (s.name.as_str(), s.kind.as_str()))
            .collect::<Vec<_>>(),
        vec![("foo", "function"), ("main", "function")]
    );

    let found = p.manager.workspace_symbols("mai", None).await.unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].name, "main");
}

#[tokio::test]
async fn rename_is_a_preview_and_touches_nothing() {
    let p = project();
    let text = "fn foo() {}\nfn main() { foo(); }\n";
    let a = p.write("a.rs", text);
    let preview = p
        .manager
        .rename(&Target::new(&a, 1).symbol("foo"), "bar")
        .await
        .unwrap();
    assert_eq!(preview.value.edit_count(), 2);
    assert_eq!(
        preview.value.files[0].preview[1].after,
        "fn main() { bar(); }"
    );
    assert_eq!(std::fs::read_to_string(&a).unwrap(), text);
}

#[tokio::test]
async fn call_hierarchy_with_no_result_is_empty_not_an_error() {
    let p = project();
    let a = p.write("a.rs", "fn foo() {}\n");
    let answer = p
        .manager
        .call_hierarchy(&Target::new(&a, 1).symbol("foo"), CallDirection::Incoming)
        .await
        .unwrap();
    assert_eq!(answer.value, Vec::new());
}

#[tokio::test]
async fn files_without_a_server_get_a_useful_error() {
    let p = project_with(false, 3);
    let a = p.write("a.rs", "fn foo() {}\n");
    let error = p
        .manager
        .definition(&Target::new(&a, 1).symbol("foo"))
        .await
        .unwrap_err();
    let message = error.to_string();
    assert!(message.contains("install one on PATH"), "{message}");
    assert!(message.contains("rust-analyzer"), "{message}");
    let report = p.manager.diagnostics_after_edit(&a).await;
    assert_eq!(report.status, ReportStatus::NoServer);
    assert_eq!(
        render_summary(&report, &RenderOptions::new(p.manager.workspace_root())),
        ""
    );

    let notes = p.write("notes.txt", "hello");
    let error = p.manager.hover(&Target::new(&notes, 1)).await.unwrap_err();
    assert!(error.to_string().contains("no language server"), "{error}");
}

#[tokio::test]
async fn a_crashed_server_is_restarted_with_backoff_and_then_given_up_on() {
    let p = project_with(true, 1);
    let a = p.write("a.rs", "fn foo() {}\n");
    p.manager
        .definition(&Target::new(&a, 1).symbol("foo"))
        .await
        .unwrap();
    assert_eq!(p.probe.launches.load(Ordering::SeqCst), 1);

    // The fake dies when it is sent CRASH.
    p.manager
        .notify_file_changed(&a, "CRASH\n".to_owned())
        .await
        .unwrap();
    let mut seen_failed = false;
    for _ in 0..100 {
        let status = p.manager.status().await;
        if matches!(
            status.first().map(|s| &s.state),
            Some(nori_lsp::types::ServerState::Failed { .. })
        ) {
            seen_failed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(seen_failed, "the crash was not noticed");

    // After the backoff the next call starts a fresh server. The file on disk is still fine.
    tokio::time::sleep(Duration::from_millis(80)).await;
    p.manager
        .definition(&Target::new(&a, 1).symbol("foo"))
        .await
        .unwrap();
    assert_eq!(p.probe.launches.load(Ordering::SeqCst), 2);

    // Crash again: the budget (one restart) is spent, so the failure is final.
    p.manager
        .notify_file_changed(&a, "CRASH\n".to_owned())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;
    let error = p
        .manager
        .definition(&Target::new(&a, 1).symbol("foo"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("not restarted"), "{error}");
    assert_eq!(p.probe.launches.load(Ordering::SeqCst), 2);

    // A manual restart gives it a fresh budget.
    assert_eq!(p.manager.restart(Some("rust-analyzer")).await, 1);
    p.manager
        .definition(&Target::new(&a, 1).symbol("foo"))
        .await
        .unwrap();
    assert_eq!(p.probe.launches.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn shutdown_stops_the_servers() {
    let p = project();
    let a = p.write("a.rs", "fn foo() {}\n");
    p.manager
        .definition(&Target::new(&a, 1).symbol("foo"))
        .await
        .unwrap();
    p.manager.shutdown().await;
    assert_eq!(p.probe.exits.load(Ordering::SeqCst), 1);
    let error = p
        .manager
        .definition(&Target::new(&a, 1).symbol("foo"))
        .await
        .unwrap_err();
    assert!(matches!(error, LspError::ShutDown));
}

#[tokio::test]
async fn tools_answer_in_text() {
    let p = project();
    let a = p.write("a.rs", "fn foo() {}\nfn main() { foo(); }\n");

    let names: Vec<&str> = tools::specs().iter().map(|spec| spec.name).collect();
    assert_eq!(names, TOOL_NAMES.to_vec());
    for spec in tools::specs() {
        assert_eq!(spec.input_schema["type"], "object", "{}", spec.name);
        assert!(spec.description.contains("Example"), "{}", spec.name);
    }

    let out = tools::call(
        &p.manager,
        "lsp_definition",
        json!({ "path": "a.rs", "line": 2, "symbol": "foo" }),
    )
    .await;
    assert!(!out.is_error, "{}", out.text);
    assert!(out.text.contains("a.rs:1:4  fn foo() {}"), "{}", out.text);

    let out = tools::call(
        &p.manager,
        "lsp_references",
        json!({ "path": "a.rs", "line": 1, "symbol": "foo" }),
    )
    .await;
    assert!(
        out.text.starts_with("2 reference(s) to `foo` at a.rs:1:4"),
        "{}",
        out.text
    );
    assert!(
        out.text.contains("  2:13  fn main() { foo(); }"),
        "{}",
        out.text
    );

    let out = tools::call(
        &p.manager,
        "lsp_rename",
        json!({ "path": "a.rs", "line": 1, "symbol": "foo", "new_name": "bar" }),
    )
    .await;
    assert!(out.text.contains("Nothing was changed"), "{}", out.text);
    assert!(out.text.contains("2: fn main() { foo(); }"), "{}", out.text);

    let out = tools::call(&p.manager, "lsp_symbols", json!({ "path": "a.rs" })).await;
    assert!(out.text.contains("function foo"), "{}", out.text);

    let out = tools::call(
        &p.manager,
        "lsp_symbols",
        json!({ "query": "foo", "path": "a.rs" }),
    )
    .await;
    assert!(out.text.contains("a.rs:1:4"), "{}", out.text);

    std::fs::write(&a, "fn foo() {}\nERROR\n").unwrap();
    let out = tools::call(&p.manager, "lsp_diagnostics", json!({ "path": "a.rs" })).await;
    assert!(!out.is_error, "{}", out.text);
    assert!(
        out.text.contains("a.rs: 1 error, 0 warnings"),
        "{}",
        out.text
    );
    let out = tools::call(&p.manager, "lsp_diagnostics", json!({})).await;
    assert!(out.text.contains("1 error(s) in 1 file(s)"), "{}", out.text);
    assert!(out.text.contains("rust-analyzer"), "{}", out.text);

    let out = tools::call(&p.manager, "lsp_hover", json!({ "path": "a.rs" })).await;
    assert!(out.is_error, "line is required: {}", out.text);
    let out = tools::call(&p.manager, "lsp_nope", json!({})).await;
    assert!(
        out.is_error && out.text.contains("lsp_diagnostics"),
        "{}",
        out.text
    );
}

#[test]
fn tool_futures_can_run_on_any_thread() {
    fn assert_send<T: Send>(_: &T) {}
    drop(async {
        let p = project();
        let call = tools::call(&p.manager, "lsp_hover", json!({}));
        assert_send(&call);
        let edit = p
            .manager
            .diagnostics_after_edit(std::path::Path::new("a.rs"));
        assert_send(&edit);
    });
}
