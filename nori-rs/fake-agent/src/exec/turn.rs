//! One prompt, from the scene it matches to the response it gets.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::Ordering;

use regex::Regex;

use nori_acp_kit::Cx;
use nori_acp_kit::Sink;
use nori_acp_kit::acp;
use nori_acp_kit::schema::ContentBlock;
use nori_acp_kit::schema::PromptRequest;
use nori_acp_kit::schema::PromptResponse;
use nori_acp_kit::schema::StopReason;

use super::Break;
use super::Ctx;
use super::Env;
use super::Lane;
use super::Live;
use super::TurnInfo;
use crate::record::Record;
use crate::session::TurnRecord;
use crate::world::CancelMode;
use crate::world::Chunking;
use crate::world::Say;
use crate::world::Step;
use crate::world::Text;
use crate::world::Unscripted;

/// Nori puts its own context ahead of the user's words (`<nori_agent_catalog>...`, and the
/// `<conversation-checkpoint>` it hands a route it fell back to). A script is written against
/// what the person said, so scenes match, and `{{prompt}}` reads, without them.
fn without_nori_context(text: &str) -> String {
    static CONTEXT: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(
                r"(?s)<(?:nori_[a-z_]+|conversation-checkpoint)>.*?</(?:nori_[a-z_]+|conversation-checkpoint)>\s*",
            )
            .ok()
    });
    match CONTEXT.as_ref() {
        Some(context) => context.replace_all(text, "").trim().to_owned(),
        None => text.trim().to_owned(),
    }
}

fn prompt_text(blocks: &[ContentBlock]) -> String {
    let text = blocks
        .iter()
        .map(|block| match block {
            ContentBlock::Text(text) => text.text.clone(),
            ContentBlock::Image(_) => "[image]".to_owned(),
            ContentBlock::Audio(_) => "[audio]".to_owned(),
            ContentBlock::ResourceLink(link) => format!("[resource: {}]", link.uri),
            _ => "[attachment]".to_owned(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    without_nori_context(&text)
}

/// Marks the session's turn as running for exactly as long as the prompt handler is.
struct TurnOpen<'a>(&'a Live);

impl<'a> TurnOpen<'a> {
    fn begin(live: &'a Live) -> Self {
        live.turn_open.store(true, Ordering::SeqCst);
        Self(live)
    }
}

impl Drop for TurnOpen<'_> {
    fn drop(&mut self) {
        self.0.turn_open.store(false, Ordering::SeqCst);
    }
}

/// Answer a `session/prompt` from the agent's scenes.
pub(crate) async fn run_prompt(
    env: &Arc<Env>,
    live: &Arc<Live>,
    cx: &Cx,
    request: PromptRequest,
) -> acp::Result<PromptResponse> {
    let turn = live.gate.begin().await?;
    let _open = TurnOpen::begin(live);
    let text = prompt_text(&request.prompt);
    let (index, model) = {
        let mut state = live.record.state();
        let index = state.prompts;
        state.prompts += 1;
        state.history.push(TurnRecord {
            prompt: text.clone(),
            updates: Vec::new(),
        });
        (
            index,
            state
                .options
                .current(nori_acp_kit::MODEL)
                .map(str::to_owned),
        )
    };
    let agent_index = env.rt.next(&format!("prompts:{}", env.def.slug));
    let scene = env.def.scenes.iter().find(|scene| {
        scene.on.matches(index, agent_index, &text)
            && scene
                .model
                .as_deref()
                .is_none_or(|wanted| model.as_deref() == Some(wanted))
    });
    env.rt.recorder.push(
        env.rt.clock.elapsed(),
        Record::PromptReceived {
            agent: env.def.slug.clone(),
            session: live.record.id.clone(),
            index,
            text: text.clone(),
            scene: scene.map(|scene| scene.name.clone().unwrap_or_else(|| "(unnamed)".to_owned())),
        },
    );

    let cancel_mode = scene.map_or(CancelMode::Honor, |scene| scene.cancel.mode);
    let participant = Arc::new(env.rt.clock.join());
    let ctx = Ctx {
        env: Arc::clone(env),
        live: Arc::clone(live),
        lane: Lane {
            id: request.session_id.clone(),
            sink: Sink::new(cx, request.session_id.clone()),
            recorded: true,
        },
        participant,
        cancel: (cancel_mode != CancelMode::Ignore).then(|| turn.token()),
        turn: Arc::new(TurnInfo {
            prompt: text.clone(),
            index,
        }),
        scope: Arc::new(BTreeMap::new()),
    };

    let result = match scene {
        Some(scene) => ctx.run(&scene.steps).await,
        None => unscripted(&ctx, index, &text).await,
    };
    let outcome = match result {
        Ok(()) => Ok(StopReason::EndTurn),
        Err(Break::Stop(reason)) => Ok(reason),
        Err(Break::Fail(error)) => Err(error),
        Err(Break::Cancelled) => {
            let policy = scene.map(|scene| &scene.cancel);
            noticing_cancel(&ctx, policy).await
        }
        Err(Break::Hang | Break::Crashed) => {
            // Never answers; the end of the connection ends it.
            ctx.forever().await;
            Err(acp::Error::internal_error())
        }
        Err(Break::Gone) => Err(acp::Error::internal_error().data("the connection ended")),
    };
    ctx.record(Record::TurnEnded {
        agent: env.def.slug.clone(),
        session: live.record.id.clone(),
        outcome: match &outcome {
            Ok(reason) => format!("{reason:?}"),
            Err(error) => format!("error {}: {}", i32::from(error.code), error.message),
        },
    });
    // The turn's share of the clock outlasts the handler until its response has left the wire,
    // so a tail that sleeps cannot overtake the response it follows.
    if let Some(sent) = env.wire.responded(&live.record.id) {
        let held = ctx.participant.join();
        tokio::spawn(async move {
            let _ = sent.await;
            drop(held);
        });
    }
    outcome.map(PromptResponse::new)
}

/// What the agent does about a cancel it noticed.
async fn noticing_cancel(
    ctx: &Ctx,
    policy: Option<&crate::world::CancelPolicy>,
) -> Result<StopReason, acp::Error> {
    let Some(policy) = policy else {
        return Ok(StopReason::Cancelled);
    };
    let uninterruptible = Ctx {
        cancel: None,
        ..ctx.clone()
    };
    if !policy.after.is_zero() && uninterruptible.sleep(policy.after.0).await.is_err() {
        return Err(acp::Error::internal_error());
    }
    match policy.mode {
        CancelMode::Hang => {
            ctx.forever().await;
            Err(acp::Error::internal_error())
        }
        CancelMode::Honor | CancelMode::Ignore => {
            if policy.detach_then {
                let tail = Ctx {
                    participant: Arc::new(ctx.participant.join()),
                    ..uninterruptible
                };
                let steps = policy.then.clone();
                tokio::spawn(async move {
                    let _ = tail.run(&steps).await;
                });
            } else if let Err(broke) = uninterruptible.run(&policy.then).await {
                return match broke {
                    Break::Stop(reason) => Ok(reason),
                    Break::Fail(error) => Err(error),
                    Break::Cancelled | Break::Hang | Break::Crashed | Break::Gone => {
                        Err(acp::Error::internal_error())
                    }
                };
            }
            Ok(StopReason::Cancelled)
        }
    }
}

async fn unscripted(ctx: &Ctx, index: i64, text: &str) -> Result<(), Break> {
    let agent = ctx.agent();
    ctx.diverge(format!(
        "no scene of {agent} matches prompt #{index} ({text:?}); add a scene, or an `on = new Always {{}}` fallback"
    ));
    match ctx.env.def.unscripted {
        Unscripted::Echo => {
            let reply = Step::Say(Say {
                text: Text::Plain(format!(
                    "{agent} has no scene for prompt #{index}. It was asked: {text}"
                )),
                chunk: Chunking::default(),
            });
            ctx.run(std::slice::from_ref(&reply)).await
        }
        Unscripted::Fail => Err(Break::Fail(acp::Error::internal_error().data(
            serde_json::json!({
                "detail": format!("{agent} has no scene for prompt #{index}")
            }),
        ))),
        Unscripted::Hang => Err(Break::Hang),
    }
}
