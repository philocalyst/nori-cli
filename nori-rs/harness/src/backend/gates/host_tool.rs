//! The `host_tool` event: an agent called one of Nori's own tools.

use std::borrow::Cow;

use nori_pkl::kinds::GateEvent;
use nori_pkl::kinds::gate::HostToolDecision;
use nori_pkl::kinds::gate::Pattern;
use nori_pkl::kinds::gate::Rule;
use serde_json::Map;
use serde_json::Value;

use super::Gates;
use super::Hit;
use super::TextMatcher;
use super::record::Decided;
use super::record::brief;
use crate::backend::orchestrator::Orchestrator;

/// What a `host_tool` rule does to a call.
#[derive(Debug)]
enum Decision {
    Allow,
    Deny {
        reason: String,
    },
    Rewrite {
        set: Vec<(String, Value)>,
        append: Vec<(String, String)>,
    },
}

/// A compiled rule of a `host_tool` gate.
#[derive(Debug)]
pub(super) struct HostTool {
    tools: Vec<String>,
    arguments: Vec<(String, TextMatcher)>,
    absent: Vec<String>,
    decision: Decision,
}

impl HostTool {
    pub(super) fn compile(at: &str, rule: &Rule) -> Result<Self, String> {
        let Rule::HostTool {
            tools,
            arguments,
            absent,
            decision,
            reason,
            set,
            append,
        } = rule
        else {
            return Err(format!("{at}: not a host_tool rule"));
        };
        let arguments = arguments
            .iter()
            .map(|(name, pattern): (&String, &Pattern)| {
                TextMatcher::compile(&format!("{at}.arguments[{name:?}]"), pattern)
                    .map(|matcher| (name.clone(), matcher))
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            tools: tools.clone(),
            arguments,
            absent: absent.clone(),
            decision: match decision {
                HostToolDecision::Allow => Decision::Allow,
                HostToolDecision::Deny => Decision::Deny {
                    reason: reason.clone().unwrap_or_default(),
                },
                HostToolDecision::Rewrite => Decision::Rewrite {
                    set: set
                        .iter()
                        .map(|(name, value)| (name.clone(), value.clone()))
                        .collect(),
                    append: append
                        .iter()
                        .map(|(name, text)| (name.clone(), text.clone()))
                        .collect(),
                },
            },
        })
    }

    fn matches(&self, tool: &str, arguments: &Value) -> bool {
        (self.tools.is_empty() || self.tools.iter().any(|name| name == tool))
            && self.absent.iter().all(|name| arguments.get(name).is_none())
            && self.arguments.iter().all(|(name, matcher)| {
                arguments.get(name).is_some_and(|value| {
                    let text: Cow<'_, str> = match value {
                        Value::String(text) => Cow::Borrowed(text),
                        other => Cow::Owned(other.to_string()),
                    };
                    matcher.matches(&text)
                })
            })
    }

    /// `arguments` with the rewrite applied, and a description of what changed. `None` when the
    /// arguments are not an object or nothing changed.
    fn rewrite(
        set: &[(String, Value)],
        append: &[(String, String)],
        arguments: &Value,
    ) -> Option<(Value, String)> {
        let mut object: Map<String, Value> = match arguments {
            Value::Object(object) => object.clone(),
            Value::Null => Map::new(),
            Value::Bool(_) | Value::Number(_) | Value::String(_) | Value::Array(_) => return None,
        };
        let mut changes = Vec::new();
        for (name, value) in set {
            object.insert(name.clone(), value.clone());
            changes.push(format!("set {name} = {value}"));
        }
        for (name, text) in append {
            match object.get_mut(name) {
                Some(Value::String(existing)) => {
                    existing.push(' ');
                    existing.push_str(text);
                }
                Some(_) => continue,
                None => {
                    object.insert(name.clone(), Value::String(text.clone()));
                }
            }
            changes.push(format!("added {text:?} to {name}"));
        }
        (!changes.is_empty()).then(|| (Value::Object(object), changes.join(", ")))
    }
}

impl Gates {
    /// The first rule of any `host_tool` gate that matches the call to `tool` with `arguments`.
    fn check_host_tool(&self, tool: &str, arguments: &Value) -> Option<Hit<'_, HostTool>> {
        self.host_tool
            .iter()
            .find(|entry| entry.rule.matches(tool, arguments))
            .map(Hit::of)
    }

    /// What the `host_tool` gates make of a call.
    pub(super) fn judge_host_call(&self, tool: &str, arguments: Value) -> Judged<'_> {
        let Some(hit) = self.check_host_tool(tool, &arguments) else {
            return Judged::Unmatched(arguments);
        };
        match &hit.rule.decision {
            Decision::Allow => Judged::Decided {
                call: GatedCall {
                    arguments,
                    rewritten: None,
                },
                gate: hit.gate,
                rule: hit.index,
                decision: "allow",
                detail: None,
                line: None,
            },
            Decision::Deny { reason } => Judged::Refused {
                gate: hit.gate,
                rule: hit.index,
                reason: reason.clone(),
            },
            Decision::Rewrite { set, append } => match HostTool::rewrite(set, append, &arguments) {
                Some((rewritten, changes)) => Judged::Decided {
                    call: GatedCall {
                        arguments: rewritten,
                        rewritten: Some(format!(
                            "Note: gate `{}` rewrote this call before it ran ({changes}).",
                            hit.gate
                        )),
                    },
                    gate: hit.gate,
                    rule: hit.index,
                    decision: "rewrite",
                    line: Some(format!(
                        "gate {}: rewrote {tool} ({})",
                        hit.gate,
                        brief(&changes)
                    )),
                    detail: Some(changes),
                },
                None => Judged::Unmatched(arguments),
            },
        }
    }
}

/// What the gates made of a call.
pub(super) enum Judged<'a> {
    /// No rule applies (or a rewrite had nothing to change): run it as it is.
    Unmatched(Value),
    /// A rule let it through or rewrote it.
    Decided {
        call: GatedCall,
        gate: &'a str,
        rule: i64,
        decision: &'static str,
        detail: Option<String>,
        line: Option<String>,
    },
    /// A rule refused it.
    Refused {
        gate: &'a str,
        rule: i64,
        reason: String,
    },
}

/// A call after the gates: what to run, and what to tell the agent about it.
#[derive(Debug)]
pub(crate) struct GatedCall {
    /// The arguments to run the tool with.
    pub(crate) arguments: Value,
    /// Set when the gates changed the arguments: what the agent's result says about it.
    pub(crate) rewritten: Option<String>,
}

/// Consult the `host_tool` gates about a call of `tool` by an agent of `orchestrator`'s session.
/// `Err` is what the tool returns as its error: the call is refused and must not run. The
/// decision is journaled (and shown, when it is a refusal or a rewrite).
pub(crate) async fn gate_host_call(
    orchestrator: &Orchestrator,
    tool: &str,
    arguments: Value,
) -> Result<GatedCall, String> {
    let gates = orchestrator.seed().tree.gates.current().await;
    if !gates.has_host_tool() {
        return Ok(GatedCall {
            arguments,
            rewritten: None,
        });
    }
    let subject = format!("{tool} {}", brief(&arguments.to_string()));
    let backend = orchestrator.inner().backend();
    match gates.judge_host_call(tool, arguments) {
        Judged::Unmatched(arguments) => Ok(GatedCall {
            arguments,
            rewritten: None,
        }),
        Judged::Decided {
            call,
            gate,
            rule,
            decision,
            detail,
            line,
        } => {
            if let Some(backend) = backend {
                let mut decided = Decided::new(GateEvent::HostTool, gate, rule, decision, &subject);
                decided.detail = detail.as_deref();
                decided.line = line;
                backend.record_gate(decided).await;
            }
            Ok(call)
        }
        Judged::Refused { gate, rule, reason } => {
            if let Some(backend) = backend {
                let line = format!("gate {gate}: denied {}", brief(&subject));
                backend
                    .record_gate(
                        Decided::new(GateEvent::HostTool, gate, rule, "deny", &subject)
                            .detail(&reason)
                            .line(line),
                    )
                    .await;
            }
            Err(format!(
                "{reason}\n\n(refused by gate `{gate}`; the call did not run)"
            ))
        }
    }
}
