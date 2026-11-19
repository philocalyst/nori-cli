//! The wire tap: where the faults that live below the ACP library happen.
//!
//! The kit's `serve` answers every request exactly once, in order, with the id it was asked
//! with. Faults that break that cannot be written as an agent: a response that vanishes, one that
//! comes twice, one for a request id that was already answered, a line that is not JSON-RPC, a
//! connection that dies mid-turn, an `initialize` that takes forever. The tap sits between the
//! host's pipe and the kit's, forwards lines both ways, and applies the faults a script armed.
//!
//! It reads lines, not the ACP types, so it knows nothing of the kit and breaks nothing in it.
//! What the agent says as updates and requests passes through unchanged and in order.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::clock::Clock;
use crate::record::FlightRecorder;
use crate::record::Record;
use crate::record::WireFate;
use crate::world::ErrorSpec;

/// What to do to the response of the next prompt of a session.
#[derive(Debug, Clone, PartialEq)]
pub enum ResponseFault {
    /// Swallow it.
    Drop,
    /// Send it, then send it again `gap` later, with another stop reason if given.
    Twice {
        gap: Duration,
        second_stop_reason: Option<String>,
    },
    /// Send it, then `gap` later answer a request id that is not pending.
    Stale { previous: bool, gap: Duration },
}

enum Writer {
    Line(String),
    /// Bytes as they are, with no line end: a partial line.
    Raw(String),
    /// Stop writing and close standard output; the process lives on.
    CloseStdout,
    /// Write no faster than this many bytes a second from now on.
    Throttle(i64),
    Close,
}

/// How the tap ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TapEnd {
    /// Either side closed the pipe.
    Closed,
    /// A script crashed the agent; a served process exits with this code.
    Crashed {
        exit_code: i64,
        message: Option<String>,
        /// Killed by SIGKILL instead of exiting.
        sigkill: bool,
    },
}

struct Pending {
    method: String,
    session: Option<String>,
}

/// What the tap needs to know about the agent it fronts.
pub struct TapContext {
    pub agent: String,
    pub recorder: FlightRecorder,
    pub clock: Clock,
    pub initialize_delay: Duration,
    pub initialize_error: Option<ErrorSpec>,
    /// The agent is a served process: it has a standard error and a pid of its own.
    pub external: bool,
}

struct State {
    context: TapContext,
    writer: mpsc::UnboundedSender<Writer>,
    faults: Mutex<HashMap<String, VecDeque<ResponseFault>>>,
    pending: Mutex<HashMap<String, Pending>>,
    answered: Mutex<HashMap<String, Value>>,
    stale_ids: AtomicI64,
    crash: CancellationToken,
    exit_code: AtomicI64,
    crash_sigkill: std::sync::atomic::AtomicBool,
    crash_message: Mutex<Option<String>>,
    /// Tasks waiting for the next prompt response of a session to leave the tap.
    responded: Mutex<HashMap<String, Vec<oneshot::Sender<()>>>>,
}

/// The handle a script's steps use to arm faults. Cheap to clone. A [`Wire::detached`] wire has
/// no tap behind it: arming a fault reports that it did nothing.
#[derive(Clone)]
pub struct Wire {
    state: Option<Arc<State>>,
}

impl std::fmt::Debug for Wire {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Wire")
            .field("interposed", &self.state.is_some())
            .finish()
    }
}

impl Wire {
    /// A wire with no tap, for an agent served without one.
    pub fn detached() -> Self {
        Self { state: None }
    }

    pub fn is_interposed(&self) -> bool {
        self.state.is_some()
    }

    /// Apply `fault` to the next prompt response of `session`. False when there is no tap.
    pub fn arm(&self, session: &str, fault: ResponseFault) -> bool {
        let Some(state) = &self.state else {
            return false;
        };
        state
            .faults
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(session.to_owned())
            .or_default()
            .push_back(fault);
        true
    }

    /// Write `line` to the client verbatim, between what the agent sent before and after. False
    /// when there is no tap.
    pub fn noise(&self, line: String, newline: bool) -> bool {
        let Some(state) = &self.state else {
            return false;
        };
        let _ = state.writer.send(if newline {
            Writer::Line(line)
        } else {
            Writer::Raw(line)
        });
        true
    }

    /// Close the agent's standard output; the process (or agent) lives on. False when there is
    /// no tap.
    pub fn close_stdout(&self) -> bool {
        let Some(state) = &self.state else {
            return false;
        };
        let _ = state.writer.send(Writer::CloseStdout);
        true
    }

    /// Slow everything written to the client from now on. False when there is no tap.
    pub fn throttle(&self, bytes_per_second: i64) -> bool {
        let Some(state) = &self.state else {
            return false;
        };
        let _ = state.writer.send(Writer::Throttle(bytes_per_second));
        true
    }

    /// The agent is a served process.
    pub fn is_external(&self) -> bool {
        self.state
            .as_ref()
            .is_some_and(|state| state.context.external)
    }

    /// Completes when the next prompt response of `session` has left the tap (or been dropped by
    /// a fault). `None` when there is no tap: nothing to wait for.
    pub fn responded(&self, session: &str) -> Option<oneshot::Receiver<()>> {
        let state = self.state.as_ref()?;
        let (done, wait) = oneshot::channel();
        state
            .responded
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(session.to_owned())
            .or_default()
            .push(done);
        Some(wait)
    }

    /// Close the connection now. False when there is no tap.
    pub fn crash(&self, exit_code: i64, message: Option<String>, sigkill: bool) -> bool {
        let Some(state) = &self.state else {
            return false;
        };
        state.crash_sigkill.store(sigkill, Ordering::SeqCst);
        *state
            .crash_message
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = message;
        state.exit_code.store(exit_code, Ordering::SeqCst);
        state.crash.cancel();
        true
    }
}

impl State {
    fn record(&self, record: Record) {
        self.context
            .recorder
            .push(self.context.clock.elapsed(), record);
    }

    fn write(&self, line: String) {
        let _ = self.writer.send(Writer::Line(line));
    }

    /// Write `line` after `gap` of the world's time.
    fn write_later(self: &Arc<Self>, gap: Duration, line: String) {
        let participant = self.context.clock.join();
        let state = Arc::clone(self);
        tokio::spawn(async move {
            participant.sleep(gap).await;
            state.write(line);
        });
    }

    fn observe_incoming(&self, line: &str) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let (Some(method), Some(id)) =
            (value.get("method").and_then(Value::as_str), value.get("id"))
        else {
            return;
        };
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                id.to_string(),
                Pending {
                    method: method.to_owned(),
                    session: value
                        .pointer("/params/sessionId")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                },
            );
    }

    fn handle_outgoing(self: &Arc<Self>, line: String) {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            self.write(line);
            return;
        };
        let is_response = value.get("method").is_none()
            && value.get("id").is_some()
            && (value.get("result").is_some() || value.get("error").is_some());
        if !is_response {
            self.write(line);
            return;
        }
        let id = value.get("id").cloned().unwrap_or(Value::Null);
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&id.to_string());
        let Some(pending) = pending else {
            self.write(line);
            return;
        };
        match pending.method.as_str() {
            "initialize" => self.initialize_response(&id, line),
            "session/prompt" => {
                self.prompt_response(&id, pending.session.as_deref(), line);
                let waiters = pending.session.and_then(|session| {
                    self.responded
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .remove(&session)
                });
                for waiter in waiters.into_iter().flatten() {
                    let _ = waiter.send(());
                }
            }
            _ => self.write(line),
        }
    }

    fn initialize_response(self: &Arc<Self>, id: &Value, line: String) {
        let line = match &self.context.initialize_error {
            Some(error) => {
                let error = crate::exec::errors::acp_error(error, &self.context.clock);
                json!({"jsonrpc": "2.0", "id": id, "error": serde_json::to_value(error).unwrap_or(Value::Null)})
                    .to_string()
            }
            None => line,
        };
        if self.context.initialize_delay.is_zero() {
            self.write(line);
        } else {
            self.write_later(self.context.initialize_delay, line);
        }
    }

    fn prompt_response(self: &Arc<Self>, id: &Value, session: Option<&str>, line: String) {
        let faults: Vec<ResponseFault> = session
            .and_then(|session| {
                self.faults
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(session)
            })
            .map(Vec::from)
            .unwrap_or_default();
        let dropped = faults.contains(&ResponseFault::Drop);
        let fate = if dropped {
            WireFate::Dropped
        } else if faults
            .iter()
            .any(|fault| matches!(fault, ResponseFault::Twice { .. }))
        {
            WireFate::Duplicated
        } else {
            WireFate::Delivered
        };
        self.record(Record::Response {
            agent: self.context.agent.clone(),
            request_id: id.clone(),
            method: "session/prompt".to_owned(),
            fate,
        });
        let previous = session.and_then(|session| {
            self.answered
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(session)
                .cloned()
        });
        if !dropped {
            if let Some(session) = session {
                self.answered
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(session.to_owned(), id.clone());
            }
            self.write(line.clone());
        }
        for fault in faults {
            match fault {
                ResponseFault::Drop => {}
                ResponseFault::Twice {
                    gap,
                    second_stop_reason,
                } => {
                    let mut again = serde_json::from_str::<Value>(&line).unwrap_or(Value::Null);
                    if let Some(reason) = second_stop_reason
                        && let Some(result) = again.get_mut("result").and_then(Value::as_object_mut)
                    {
                        result.insert("stopReason".to_owned(), json!(reason));
                    }
                    self.write_later(gap, again.to_string());
                }
                ResponseFault::Stale {
                    previous: use_previous,
                    gap,
                } => {
                    let stale_id = match (use_previous, &previous) {
                        (true, Some(previous)) => previous.clone(),
                        (true, None) | (false, _) => {
                            json!(9_000_000 + self.stale_ids.fetch_add(1, Ordering::SeqCst))
                        }
                    };
                    self.record(Record::Response {
                        agent: self.context.agent.clone(),
                        request_id: stale_id.clone(),
                        method: "session/prompt".to_owned(),
                        fate: WireFate::Stale,
                    });
                    let stale = json!({
                        "jsonrpc": "2.0",
                        "id": stale_id,
                        "result": {"stopReason": "end_turn"},
                    });
                    self.write_later(gap, stale.to_string());
                }
            }
        }
    }
}

/// The running half of a tap: forwards lines until a side closes or a script crashes the agent.
pub struct Tap {
    state: Arc<State>,
    host_read: Box<dyn AsyncRead + Send + Unpin>,
    host_write: Box<dyn AsyncWrite + Send + Unpin>,
    tap_end: tokio::io::DuplexStream,
    writer_rx: mpsc::UnboundedReceiver<Writer>,
}

/// Put a tap between the host's side of the pipe and the kit. Returns the wire scripts arm
/// faults on, the end to give the kit's `serve_over_pipe_as`, and the tap to run beside it.
pub fn interpose(
    host: impl AsyncRead + AsyncWrite + Send + Unpin + 'static,
    context: TapContext,
) -> (Wire, tokio::io::DuplexStream, Tap) {
    let (host_read, host_write) = tokio::io::split(host);
    let (kit_end, tap_end) = tokio::io::duplex(1 << 16);
    let (writer, writer_rx) = mpsc::unbounded_channel();
    let state = Arc::new(State {
        context,
        writer,
        faults: Mutex::default(),
        pending: Mutex::default(),
        answered: Mutex::default(),
        stale_ids: AtomicI64::new(0),
        crash: CancellationToken::new(),
        exit_code: AtomicI64::new(0),
        crash_sigkill: std::sync::atomic::AtomicBool::new(false),
        crash_message: Mutex::new(None),
        responded: Mutex::default(),
    });
    (
        Wire {
            state: Some(Arc::clone(&state)),
        },
        kit_end,
        Tap {
            state,
            host_read: Box::new(host_read),
            host_write: Box::new(host_write),
            tap_end,
            writer_rx,
        },
    )
}

impl Tap {
    /// Forward lines both ways until a side closes or a script crashes the agent.
    pub async fn run(self) -> TapEnd {
        let Self {
            state,
            host_read,
            mut host_write,
            tap_end,
            mut writer_rx,
        } = self;
        let (kit_read, mut to_kit) = tokio::io::split(tap_end);

        let incoming = {
            let state = Arc::clone(&state);
            async move {
                let mut reader = BufReader::new(host_read);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    state.observe_incoming(&line);
                    if to_kit.write_all(line.as_bytes()).await.is_err() {
                        break;
                    }
                }
                // The client left: let the kit see the end of its input.
                let _ = to_kit.shutdown().await;
            }
        };
        let outgoing = {
            let state = Arc::clone(&state);
            async move {
                let mut reader = BufReader::new(kit_read);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => state.handle_outgoing(line.trim_end_matches('\n').to_owned()),
                    }
                }
                let _ = state.writer.send(Writer::Close);
            }
        };
        let external = state.context.external;
        let writing = async move {
            let mut rate: Option<i64> = None;
            let mut closed = false;
            while let Some(message) = writer_rx.recv().await {
                let bytes = match message {
                    Writer::Line(mut line) => {
                        line.push('\n');
                        line
                    }
                    Writer::Raw(text) => text,
                    Writer::CloseStdout => {
                        // The process lives on, and what it writes goes nowhere.
                        let _ = host_write.shutdown().await;
                        closed = true;
                        // A served process closes the descriptor itself: ending the copy to it
                        // leaves the real standard output open, which is not what a process that
                        // lost its stdout looks like to the client.
                        #[cfg(unix)]
                        if external {
                            tokio::time::sleep(Duration::from_millis(150)).await;
                            // SAFETY: closing descriptor 1 has no memory-safety preconditions;
                            // nothing writes to it afterwards.
                            unsafe {
                                libc::close(1);
                            }
                        }
                        continue;
                    }
                    Writer::Throttle(bytes_per_second) => {
                        rate = Some(bytes_per_second.max(1));
                        continue;
                    }
                    Writer::Close => break,
                };
                if closed {
                    continue;
                }
                // A throttled pipe takes its bytes a chunk at a time.
                let chunk = rate.map_or(bytes.len().max(1), |rate| {
                    usize::try_from((rate / 20).max(1)).unwrap_or(1)
                });
                let mut failed = false;
                for piece in bytes.as_bytes().chunks(chunk) {
                    if host_write.write_all(piece).await.is_err()
                        || host_write.flush().await.is_err()
                    {
                        failed = true;
                        break;
                    }
                    if let Some(rate) = rate {
                        let pause = Duration::from_secs_f64(piece.len() as f64 / rate as f64);
                        tokio::time::sleep(pause).await;
                    }
                }
                if failed {
                    break;
                }
            }
            let _ = host_write.shutdown().await;
        };
        let crash = state.crash.clone();
        tokio::select! {
            () = async { tokio::join!(incoming, outgoing, writing); } => TapEnd::Closed,
            () = crash.cancelled() => {
                let exit_code = state.exit_code.load(Ordering::SeqCst);
                state.record(Record::Crashed {
                    agent: state.context.agent.clone(),
                    exit_code,
                });
                let message = state
                    .crash_message
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take();
                let sigkill = state.crash_sigkill.load(Ordering::SeqCst);
                TapEnd::Crashed {
                    exit_code,
                    message,
                    sigkill,
                }
            }
        }
    }
}
