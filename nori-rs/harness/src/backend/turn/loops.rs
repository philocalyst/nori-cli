//! Loop detection: an agent that makes the same tool call again and again is not getting
//! anywhere, and every round costs tokens.
//!
//! Every tool call a turn sees is kept with a typed signature: its kind, its title, and a stable
//! hash of its raw input. When one signature fills more than `repeats` of the last `window` calls
//! of a turn, the policy's [`LoopAction`] decides: steer the agent with one short message (the
//! turn is interrupted and the message is the next prompt, as a typed-ahead prompt steers), and
//! when it loops again in the same task, cancel the turn as the user's Esc does.
//!
//! A "task" is the work from one user prompt to the next, so a nudge is remembered across the
//! turn it interrupts and the turn it starts. The windows are per turn: after a nudge the agent
//! has the whole threshold to show it changed course.
//!
//! The idea follows OpenCode's doom-loop check (MIT, `sst/opencode`): the same tool with the
//! same input, repeated. The window and the policy are Nori's own.
//!
//! A call whose input has not been seen is never counted, so parallel calls that are still
//! streaming their arguments cannot look like repeats of one another.

use std::collections::VecDeque;
use std::hash::Hash;
use std::hash::Hasher;

use nori_protocol::acp::v1 as acp;
use nori_workflow::LoopAction;
use nori_workflow::LoopPolicy;

use super::super::session_reducer::ReduceOutput;
use super::cancel;
use super::queue;
use crate::normalized::ClientEvent;
use crate::normalized::WarningInfo;
use crate::normalized::session_runtime::QueuedPrompt;
use crate::normalized::session_runtime::QueuedPromptKind;
use crate::normalized::session_runtime::SessionRuntime;

/// One tool call of a turn, as far as its signature is known.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TrackedCall {
    call_id: String,
    kind: acp::ToolKind,
    title: String,
    /// Hash of the raw input; `None` until the agent has sent it.
    input: Option<u64>,
}

impl TrackedCall {
    fn same_signature(&self, other: &Self) -> bool {
        self.input.is_some()
            && self.kind == other.kind
            && self.title == other.title
            && self.input == other.input
    }
}

/// What the agent said about a tool call this time: a new call, or an update of a known one.
#[derive(Debug, Clone, Copy)]
pub(in crate::backend) struct CallObservation<'a> {
    pub call_id: &'a str,
    pub kind: Option<acp::ToolKind>,
    pub title: Option<&'a str>,
    pub raw_input: Option<&'a serde_json::Value>,
}

/// A signature that filled more of the window than the policy allows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::backend) struct LoopFound {
    /// The repeated call's title, such as `cargo test`.
    pub title: String,
    /// How often the signature is in the window.
    pub count: i64,
}

/// The latest tool calls of one turn.
#[derive(Debug, Clone, Default)]
pub struct CallWindow {
    calls: VecDeque<TrackedCall>,
}

impl CallWindow {
    /// Take in what the agent said about a call. Returns the loop the call completed, if it did:
    /// a loop is reported when a call's signature becomes known, once, never on later updates.
    pub(in crate::backend) fn observe(
        &mut self,
        policy: &LoopPolicy,
        observation: CallObservation<'_>,
    ) -> Option<LoopFound> {
        let position = self
            .calls
            .iter()
            .position(|call| call.call_id == observation.call_id)
            .unwrap_or_else(|| {
                self.calls.push_back(TrackedCall {
                    call_id: observation.call_id.to_owned(),
                    kind: acp::ToolKind::default(),
                    title: String::new(),
                    input: None,
                });
                self.calls.len() - 1
            });
        let window = usize::try_from(policy.window).unwrap_or(usize::MAX);
        let evicted = self.calls.len().saturating_sub(window);
        self.calls.drain(..evicted);
        let position = position.checked_sub(evicted)?;

        let call = &mut self.calls[position];
        if let Some(kind) = observation.kind {
            call.kind = kind;
        }
        if let Some(title) = observation.title {
            call.title = title.to_owned();
        }
        let previous = call.input;
        if let Some(input) = observation.raw_input {
            call.input = Some(stable_hash(input));
        }
        if call.input.is_none() || call.input == previous {
            return None;
        }
        let call = call.clone();
        let count = self
            .calls
            .iter()
            .filter(|other| call.same_signature(other))
            .count();
        let count = i64::try_from(count).unwrap_or(i64::MAX);
        (count > policy.repeats).then_some(LoopFound {
            title: call.title,
            count,
        })
    }
}

/// A hash of `value` that does not depend on the order of an object's keys.
fn stable_hash(value: &serde_json::Value) -> u64 {
    fn feed(value: &serde_json::Value, hasher: &mut std::hash::DefaultHasher) {
        match value {
            serde_json::Value::Null => 0u8.hash(hasher),
            serde_json::Value::Bool(flag) => (1u8, flag).hash(hasher),
            serde_json::Value::Number(number) => (2u8, number.to_string()).hash(hasher),
            serde_json::Value::String(text) => (3u8, text).hash(hasher),
            serde_json::Value::Array(items) => {
                (4u8, items.len()).hash(hasher);
                for item in items {
                    feed(item, hasher);
                }
            }
            serde_json::Value::Object(entries) => {
                (5u8, entries.len()).hash(hasher);
                let mut sorted: Vec<_> = entries.iter().collect();
                sorted.sort_by_key(|(key, _)| key.as_str());
                for (key, item) in sorted {
                    key.hash(hasher);
                    feed(item, hasher);
                }
            }
        }
    }
    let mut hasher = std::hash::DefaultHasher::new();
    feed(value, &mut hasher);
    hasher.finish()
}

/// What to do about a loop that was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// Steer the agent.
    Nudge,
    /// Cancel the turn.
    Stop,
    /// Say nothing and let it go on.
    Ignore,
}

/// The session's loop policy and what it has already done in the current task.
#[derive(Debug, Clone, Default)]
pub struct LoopGuard {
    pub policy: LoopPolicy,
    /// The agent was steered in the current task.
    nudged: bool,
}

impl LoopGuard {
    /// A user prompt starts a new task: the agent may be steered again.
    pub(in crate::backend) fn new_task(&mut self) {
        self.nudged = false;
    }

    fn verdict(&mut self) -> Verdict {
        let nudge = |nudged: &mut bool| {
            if std::mem::replace(nudged, true) {
                None
            } else {
                Some(Verdict::Nudge)
            }
        };
        match self.policy.action {
            LoopAction::Off => Verdict::Ignore,
            LoopAction::Stop => Verdict::Stop,
            LoopAction::Nudge => nudge(&mut self.nudged).unwrap_or(Verdict::Ignore),
            LoopAction::NudgeThenStop => nudge(&mut self.nudged).unwrap_or(Verdict::Stop),
        }
    }
}

/// Act on a loop the open turn just completed: steer the agent or cancel the turn, and tell the
/// person in one line.
pub(in crate::backend) fn react(
    runtime: &mut SessionRuntime,
    found: &LoopFound,
    out: &mut ReduceOutput,
) {
    let LoopFound { title, count } = found;
    match runtime.loops.verdict() {
        Verdict::Ignore => {}
        Verdict::Nudge => {
            out.events.push(ClientEvent::Warning(WarningInfo {
                message: format!(
                    "Loop: \"{title}\" called {count} times with the same input; steering the agent."
                ),
            }));
            let text = format!(
                "You have called \"{title}\" with the same input {count} times; it is not making progress. Stop and say what is blocking you, or try a different approach."
            );
            queue::steer(
                runtime,
                QueuedPrompt {
                    event_id: format!("loop-nudge-{}", uuid::Uuid::new_v4()),
                    kind: QueuedPromptKind::LoopNudge,
                    text: text.clone(),
                    prompt_meta: None,
                    original_content: Vec::new(),
                    content: vec![acp::ContentBlock::Text(acp::TextContent::new(text))],
                    context: Default::default(),
                    display_text: None,
                },
                out,
            );
        }
        Verdict::Stop => {
            out.events.push(ClientEvent::Warning(WarningInfo {
                message: format!(
                    "Loop: \"{title}\" called {count} times with the same input; stopped the turn."
                ),
            }));
            cancel::cancel(runtime, None, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::session_reducer::InboundEvent;
    use crate::backend::session_reducer::SideEffect;
    use crate::backend::session_reducer::reduce;
    use crate::normalized::ClientEventNormalizer;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn call<'a>(id: &'a str, title: &'a str, input: &'a serde_json::Value) -> CallObservation<'a> {
        CallObservation {
            call_id: id,
            kind: Some(acp::ToolKind::Execute),
            title: Some(title),
            raw_input: Some(input),
        }
    }

    #[test]
    fn a_signature_is_a_loop_only_above_the_repeat_count() {
        let policy = LoopPolicy::default();
        let input = json!({ "command": "cargo test" });
        let mut window = CallWindow::default();
        let found: Vec<_> = (0..8)
            .map(|index| window.observe(&policy, call(&format!("c{index}"), "cargo test", &input)))
            .collect();
        assert_eq!(found[..5], [None, None, None, None, None]);
        assert_eq!(
            found[5],
            Some(LoopFound {
                title: "cargo test".to_owned(),
                count: 6
            })
        );
        assert_eq!(found[6].as_ref().map(|found| found.count), Some(7));
    }

    #[test]
    fn different_input_title_or_kind_is_a_different_signature() {
        let policy = LoopPolicy {
            window: 10,
            repeats: 2,
            ..LoopPolicy::default()
        };
        let mut window = CallWindow::default();
        let (a, b) = (json!({ "path": "a" }), json!({ "path": "b" }));
        assert_eq!(window.observe(&policy, call("1", "Read", &a)), None);
        assert_eq!(window.observe(&policy, call("2", "Read", &b)), None);
        assert_eq!(window.observe(&policy, call("3", "Edit", &a)), None);
        assert_eq!(window.observe(&policy, call("4", "Read", &a)), None);
        let other_kind = CallObservation {
            kind: Some(acp::ToolKind::Read),
            ..call("5", "Read", &a)
        };
        assert_eq!(window.observe(&policy, other_kind), None);
        // Calls 1, 4 and 6 share kind, title and input; call 5 differs in kind.
        assert_eq!(
            window.observe(&policy, call("6", "Read", &a)),
            Some(LoopFound {
                title: "Read".to_owned(),
                count: 3
            })
        );
    }

    #[test]
    fn object_key_order_does_not_change_a_signature() {
        let one: serde_json::Value =
            serde_json::from_str(r#"{"a":1,"b":{"x":[1,2],"y":null}}"#).unwrap();
        let two: serde_json::Value =
            serde_json::from_str(r#"{"b":{"y":null,"x":[1,2]},"a":1}"#).unwrap();
        let other: serde_json::Value =
            serde_json::from_str(r#"{"a":1,"b":{"x":[2,1],"y":null}}"#).unwrap();
        assert_eq!(stable_hash(&one), stable_hash(&two));
        assert!(stable_hash(&one) != stable_hash(&other));
        assert!(stable_hash(&json!("1")) != stable_hash(&json!(1)));
    }

    #[test]
    fn calls_older_than_the_window_are_forgotten() {
        let policy = LoopPolicy {
            window: 4,
            repeats: 2,
            ..LoopPolicy::default()
        };
        let same = json!("same");
        let mut window = CallWindow::default();
        assert_eq!(window.observe(&policy, call("1", "Run", &same)), None);
        assert_eq!(window.observe(&policy, call("2", "Run", &same)), None);
        for index in 3..7 {
            assert_eq!(
                window.observe(&policy, call(&index.to_string(), "Run", &json!(index))),
                None
            );
        }
        // The two early repeats have left the window of four.
        assert_eq!(window.observe(&policy, call("7", "Run", &same)), None);
    }

    #[test]
    fn a_call_counts_once_its_input_arrives_and_updates_do_not_recount_it() {
        let policy = LoopPolicy {
            window: 10,
            repeats: 1,
            ..LoopPolicy::default()
        };
        let input = json!({ "q": 1 });
        let mut window = CallWindow::default();
        let announced = |id| CallObservation {
            call_id: id,
            kind: Some(acp::ToolKind::Search),
            title: Some("Search"),
            raw_input: None,
        };
        // Three parallel calls still streaming their arguments are not repeats.
        for id in ["1", "2", "3"] {
            assert_eq!(window.observe(&policy, announced(id)), None);
        }
        assert_eq!(window.observe(&policy, call("1", "Search", &input)), None);
        let second = window.observe(&policy, call("2", "Search", &input));
        assert_eq!(second.as_ref().map(|found| found.count), Some(2));
        // The same input again on the same call is an update, not another call.
        assert_eq!(window.observe(&policy, call("2", "Search", &input)), None);
    }

    #[test]
    fn the_verdict_follows_the_action_and_the_task() {
        let mut guard = LoopGuard::default();
        assert_eq!(guard.verdict(), Verdict::Nudge);
        assert_eq!(guard.verdict(), Verdict::Stop);
        guard.new_task();
        assert_eq!(guard.verdict(), Verdict::Nudge);

        let mut nudge_only = LoopGuard {
            policy: LoopPolicy {
                action: LoopAction::Nudge,
                ..LoopPolicy::default()
            },
            ..LoopGuard::default()
        };
        assert_eq!(nudge_only.verdict(), Verdict::Nudge);
        assert_eq!(nudge_only.verdict(), Verdict::Ignore);

        let mut stop = LoopGuard {
            policy: LoopPolicy {
                action: LoopAction::Stop,
                ..LoopPolicy::default()
            },
            ..LoopGuard::default()
        };
        assert_eq!(stop.verdict(), Verdict::Stop);

        let mut off = LoopGuard {
            policy: LoopPolicy {
                action: LoopAction::Off,
                ..LoopPolicy::default()
            },
            ..LoopGuard::default()
        };
        assert_eq!(off.verdict(), Verdict::Ignore);
    }

    fn tool_call(id: &str) -> InboundEvent {
        InboundEvent::Notification(Box::new(acp::SessionUpdate::ToolCall(
            acp::ToolCall::new(acp::ToolCallId::new(id), "cargo test")
                .kind(acp::ToolKind::Execute)
                .raw_input(json!({ "command": "cargo test" })),
        )))
    }

    fn user_prompt() -> QueuedPrompt {
        QueuedPrompt {
            event_id: "user-1".to_owned(),
            kind: QueuedPromptKind::User,
            text: "fix it".to_owned(),
            prompt_meta: None,
            original_content: Vec::new(),
            content: Vec::new(),
            context: Default::default(),
            display_text: None,
        }
    }

    /// Drive six identical calls through the real reducer.
    fn loop_through_reducer(runtime: &mut SessionRuntime) -> ReduceOutput {
        let mut normalizer = ClientEventNormalizer::default();
        let mut last = ReduceOutput::default();
        for index in 0..6 {
            last = reduce(runtime, tool_call(&format!("c{index}")), &mut normalizer);
        }
        last
    }

    #[test]
    fn the_reducer_steers_once_then_cancels_the_turn() {
        let mut runtime = SessionRuntime::new();
        let mut normalizer = ClientEventNormalizer::default();
        reduce(
            &mut runtime,
            InboundEvent::PromptSubmit(user_prompt()),
            &mut normalizer,
        );

        let first = loop_through_reducer(&mut runtime);
        assert!(
            first
                .side_effects
                .iter()
                .any(|effect| matches!(effect, SideEffect::SendCancel { .. })),
            "the turn is interrupted so the nudge can start: {:?}",
            first.side_effects
        );
        let nudge = runtime.queue.front().expect("the nudge waits to start");
        assert_eq!(nudge.kind, QueuedPromptKind::LoopNudge);
        assert_eq!(
            nudge.text,
            "You have called \"cargo test\" with the same input 6 times; it is not making progress. Stop and say what is blocking you, or try a different approach."
        );
        assert!(
            first.events.iter().any(|event| matches!(
                event,
                ClientEvent::Warning(warning) if warning.message.contains("steering the agent")
            )),
            "the person is told"
        );

        // The interrupted turn ends and the nudge turn starts; the agent loops again.
        let request_id = runtime
            .turn
            .as_ref()
            .and_then(|turn| turn.request_id().cloned())
            .expect("owner");
        let ended = reduce(
            &mut runtime,
            InboundEvent::PromptResultFor {
                request_id,
                result: crate::backend::prompt_lifecycle::PromptResult::Response(
                    acp::StopReason::Cancelled,
                ),
            },
            &mut normalizer,
        );
        assert!(
            ended
                .side_effects
                .iter()
                .any(|effect| matches!(effect, SideEffect::SendPrompt { .. })),
            "{:?}",
            ended.side_effects
        );
        let second = loop_through_reducer(&mut runtime);
        assert!(runtime.queue.is_empty(), "no second nudge");
        assert!(
            second
                .side_effects
                .iter()
                .any(|effect| matches!(effect, SideEffect::SendCancel { .. }))
        );
        assert!(
            second.events.iter().any(|event| matches!(
                event,
                ClientEvent::Warning(warning) if warning.message.contains("stopped the turn")
            )),
            "{:?}",
            second.events
        );
        assert!(
            runtime
                .turn
                .as_ref()
                .is_some_and(crate::normalized::turn::Turn::is_cancelling)
        );
    }

    #[test]
    fn a_new_user_prompt_starts_a_new_task() {
        let mut runtime = SessionRuntime::new();
        let mut normalizer = ClientEventNormalizer::default();
        runtime.loops.policy.action = LoopAction::Nudge;
        reduce(
            &mut runtime,
            InboundEvent::PromptSubmit(user_prompt()),
            &mut normalizer,
        );
        loop_through_reducer(&mut runtime);
        assert_eq!(runtime.queue.len(), 1, "steered once");
        runtime.queue.clear();
        // Cancelling turns are not examined further; end the turn and start a user task again.
        let request_id = runtime
            .turn
            .as_ref()
            .and_then(|turn| turn.request_id().cloned())
            .expect("owner");
        reduce(
            &mut runtime,
            InboundEvent::PromptResultFor {
                request_id,
                result: crate::backend::prompt_lifecycle::PromptResult::Response(
                    acp::StopReason::Cancelled,
                ),
            },
            &mut normalizer,
        );
        reduce(
            &mut runtime,
            InboundEvent::PromptSubmit(user_prompt()),
            &mut normalizer,
        );
        loop_through_reducer(&mut runtime);
        assert_eq!(runtime.queue.len(), 1, "a new task may be steered again");
    }

    #[test]
    fn a_replayed_history_is_never_a_loop() {
        let mut runtime = SessionRuntime::new();
        let mut normalizer = ClientEventNormalizer::default();
        reduce(
            &mut runtime,
            InboundEvent::LoadSubmit {
                request_id: acp::RequestId::Str("load".into()),
            },
            &mut normalizer,
        );
        let out = loop_through_reducer(&mut runtime);
        assert!(
            !out.side_effects
                .iter()
                .any(|effect| matches!(effect, SideEffect::SendCancel { .. })),
            "{:?}",
            out.side_effects
        );
        assert!(runtime.queue.is_empty());
    }
}
