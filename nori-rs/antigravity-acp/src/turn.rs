//! One `session/prompt`: send the prompt, translate the harness's steps until the turn ends.

use antigravity_sdk_rust::types::ContentPrimitive;
use antigravity_sdk_rust::types::Step;
use antigravity_sdk_rust::types::StepSource;
use antigravity_sdk_rust::types::StepStatus;
use antigravity_sdk_rust::types::StepTarget;
use antigravity_sdk_rust::types::StepType;
use antigravity_sdk_rust::types::UsageMetadata;
use futures::StreamExt;
use nori_acp_kit::ContextUsage;
use nori_acp_kit::Sink;
use nori_acp_kit::TokenBreakdown;
use nori_acp_kit::Turn;
use nori_acp_kit::UsageReport;
use nori_acp_kit::acp;
use nori_acp_kit::schema::ContentBlock;
use nori_acp_kit::schema::PromptResponse;
use nori_acp_kit::schema::StopReason;
use nori_acp_kit::schema::Usage;

use crate::errors;
use crate::models;
use crate::prompt_codec;
use crate::session::Session;
use crate::store::SessionStore;
use crate::translate::StepTranslator;

/// The context window of the Gemini 4 models. The harness does not report one.
const CONTEXT_WINDOW: i64 = 1_000_000;

/// Run one turn and persist the session however it ends.
pub(crate) async fn run(
    session: &Session,
    store: &SessionStore,
    sink: Sink,
    turn: Turn,
    prompt: Vec<ContentBlock>,
) -> acp::Result<PromptResponse> {
    let result = run_turn(session, &sink, &turn, prompt).await;
    session.persist(store).await;
    result
}

async fn run_turn(
    session: &Session,
    sink: &Sink,
    turn: &Turn,
    prompt: Vec<ContentBlock>,
) -> acp::Result<PromptResponse> {
    session.conn.attach(sink.clone());
    let content = prompt_codec::prompt_to_content(prompt)?;
    let user_text = content
        .parts()
        .into_iter()
        .filter_map(|part| match part {
            ContentPrimitive::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    if let Some(first_line) = user_text.lines().next().filter(|line| !line.is_empty()) {
        let mut title = session.title.lock().await;
        if title.is_none() {
            *title = Some(first_line.chars().take(80).collect());
        }
    }
    {
        let mut history = session.history.lock().await;
        let step_index = u32::try_from(history.len()).unwrap_or(u32::MAX);
        history.push(Step {
            id: format!("{}_user_{step_index}", session.id),
            step_index,
            r#type: StepType::TextResponse,
            source: StepSource::User,
            target: StepTarget::User,
            status: StepStatus::Done,
            content: user_text,
            ..Step::default()
        });
    }

    let model = models::model(&*session.options.lock().await);
    session.conn.send(&content, &model).await?;
    let mut steps = session.conn.receive_steps();
    let mut translator = StepTranslator::default();
    let mut usage = UsageAccumulator::default();
    loop {
        let next = tokio::select! {
            item = steps.next() => item,
            () = turn.cancelled() => return Ok(PromptResponse::new(StopReason::Cancelled)),
        };
        let Some(step) = next else {
            break;
        };
        let step = match step {
            Ok(step) => step,
            Err(error) if errors::is_cancelled(&error) || turn.is_cancelled() => {
                return Ok(PromptResponse::new(StopReason::Cancelled));
            }
            Err(error) => return Err(errors::classify(&error, &model).into()),
        };
        session.history.lock().await.push(step.clone());
        if let Some(metadata) = &step.usage_metadata {
            usage.add(metadata);
        }
        for update in translator.translate_step(&step, session.host.as_ref()) {
            sink.update(update)?;
        }
        if step.status == StepStatus::TerminalError {
            let message = if step.error.is_empty() {
                "Antigravity execution failed".to_owned()
            } else {
                step.error
            };
            return Err(errors::classify(&anyhow::anyhow!(message), &model).into());
        }
    }

    if turn.is_cancelled() {
        return Ok(PromptResponse::new(StopReason::Cancelled));
    }
    let mut response = PromptResponse::new(StopReason::EndTurn);
    if usage.total > 0 {
        sink.usage(usage.context(), &usage.report(&model))?;
        response = response.usage(usage.acp_usage());
    }
    Ok(response)
}

#[derive(Default)]
struct UsageAccumulator {
    input: u64,
    output: u64,
    thoughts: u64,
    cached: u64,
    total: u64,
    /// The prompt size of the latest request: what the context window holds now.
    last_prompt: u64,
}

impl UsageAccumulator {
    fn add(&mut self, meta: &UsageMetadata) {
        self.input = self.input.saturating_add(meta.prompt_token_count);
        self.output = self.output.saturating_add(meta.candidates_token_count);
        self.thoughts = self.thoughts.saturating_add(meta.thoughts_token_count);
        self.cached = self.cached.saturating_add(meta.cached_content_token_count);
        self.total = self.total.saturating_add(meta.total_token_count);
        self.last_prompt = meta.prompt_token_count;
    }

    fn context(&self) -> Option<ContextUsage> {
        (self.last_prompt > 0).then(|| ContextUsage {
            used: i64::try_from(self.last_prompt).unwrap_or(i64::MAX),
            size: CONTEXT_WINDOW,
        })
    }

    /// Gemini counts cached tokens inside the prompt; the report separates them.
    fn report(&self, model: &str) -> UsageReport {
        let count = |value: u64| i64::try_from(value).unwrap_or(i64::MAX);
        UsageReport::default().model(model).tokens(TokenBreakdown {
            input: count(self.input.saturating_sub(self.cached)),
            output: count(self.output),
            cache_read: count(self.cached),
            cache_write: 0,
            reasoning: count(self.thoughts),
        })
    }

    fn acp_usage(&self) -> Usage {
        Usage::new(self.total, self.input, self.output)
            .thought_tokens((self.thoughts > 0).then_some(self.thoughts))
            .cached_read_tokens((self.cached > 0).then_some(self.cached))
    }
}
