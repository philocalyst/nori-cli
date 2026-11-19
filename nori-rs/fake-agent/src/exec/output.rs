//! Text, plans, usage and session facts.

use nori_acp_kit::ContextUsage;
use nori_acp_kit::RateLimitWindow;
use nori_acp_kit::TokenBreakdown;
use nori_acp_kit::UsageReport;
use nori_acp_kit::schema::ContentBlock;
use nori_acp_kit::schema::ContentChunk;
use nori_acp_kit::schema::CurrentModeUpdate;
use nori_acp_kit::schema::Meta;
use nori_acp_kit::schema::Plan;
use nori_acp_kit::schema::PlanEntry;
use nori_acp_kit::schema::PlanEntryPriority;
use nori_acp_kit::schema::PlanEntryStatus;
use nori_acp_kit::schema::SessionInfoUpdate;
use nori_acp_kit::schema::SessionUpdate;
use serde_json::Value;
use serde_json::json;

use super::Ctx;
use super::Run;
use crate::world::ModeStep;
use crate::world::PlanStatusDef;
use crate::world::PlanStep;
use crate::world::PriorityDef;
use crate::world::Say;
use crate::world::SessionInfoStep;
use crate::world::StatusState;
use crate::world::StatusStep;
use crate::world::UsageStep;

impl Ctx {
    /// Stream the text in chunks at the scripted cadence.
    pub(super) async fn say(&self, say: &Say, thought: bool) -> Run {
        let text = self.text(&say.text);
        let chunks = say.chunk.split(&text);
        let last = chunks.len().saturating_sub(1);
        for (index, chunk) in chunks.into_iter().enumerate() {
            let block = ContentChunk::new(ContentBlock::from(chunk));
            self.emit(if thought {
                SessionUpdate::AgentThoughtChunk(block)
            } else {
                SessionUpdate::AgentMessageChunk(block)
            })?;
            if index < last && !say.chunk.every.is_zero() {
                self.sleep(say.chunk.every.0).await?;
            }
        }
        Ok(())
    }

    pub(super) fn plan(&self, plan: &PlanStep) -> Run {
        let entries = plan
            .entries
            .iter()
            .map(|entry| {
                PlanEntry::new(
                    self.render(&entry.content),
                    match entry.priority {
                        PriorityDef::High => PlanEntryPriority::High,
                        PriorityDef::Medium => PlanEntryPriority::Medium,
                        PriorityDef::Low => PlanEntryPriority::Low,
                    },
                    match entry.status {
                        PlanStatusDef::Pending => PlanEntryStatus::Pending,
                        PlanStatusDef::InProgress => PlanEntryStatus::InProgress,
                        PlanStatusDef::Completed => PlanEntryStatus::Completed,
                    },
                )
            })
            .collect();
        self.emit(SessionUpdate::Plan(Plan::new(entries)))
    }

    /// Usage with the `_meta.nori` shape of the usage contract.
    pub(super) fn usage(&self, usage: &UsageStep) -> Run {
        let clock = &self.env.rt.clock;
        let model = usage.model.clone().or_else(|| self.live.record.model());
        let mut report = UsageReport::default();
        if let Some(model) = &model {
            report = report.model(model.clone());
        }
        if let Some(account) = usage
            .account
            .clone()
            .or_else(|| self.env.def.account.clone())
        {
            report = report.account(account);
        }
        if let Some(tokens) = &usage.tokens {
            let breakdown = TokenBreakdown {
                input: tokens.input,
                output: tokens.output,
                cache_read: tokens.cache_read,
                cache_write: tokens.cache_write,
                reasoning: tokens.reasoning,
            };
            report = if usage.session_total {
                report.session_tokens(breakdown)
            } else {
                report.tokens(breakdown)
            };
        }
        for limit in &usage.rate_limits {
            let mut window = RateLimitWindow::new(limit.id.clone(), limit.label.clone())
                .used_percent(limit.used_percent)
                .window_minutes(limit.window_minutes);
            window.resets_at = limit
                .resets_at
                .clone()
                .or_else(|| limit.resets_in.map(|span| clock.rfc3339_in(span.0)));
            report = report.rate_limit(window);
        }
        let size = usage.context_size.or_else(|| {
            model
                .as_deref()
                .and_then(|id| self.env.def.model(id))
                .and_then(|model| model.context_window)
        });
        let context = usage
            .context_used
            .zip(size)
            .map(|(used, size)| ContextUsage { used, size });
        match nori_acp_kit::usage_update(context, &report) {
            Some(update) => self.emit(update),
            None => {
                self.diverge(
                    "a usage step reported nothing: give it tokens, a model, or rate limits",
                );
                Ok(())
            }
        }
    }

    pub(super) fn session_info(&self, info: &SessionInfoStep) -> Run {
        let mut update = SessionInfoUpdate::new().updated_at(self.env.rt.clock.now_rfc3339());
        if let Some(title) = &info.title {
            let title = self.render(title);
            self.live.record.state().title = Some(title.clone());
            update = update.title(title);
        }
        if let Some(Value::Object(meta)) = &info.meta {
            update = update.meta(Meta::from_iter(meta.clone()));
        }
        self.emit(SessionUpdate::SessionInfoUpdate(update))
    }

    /// Nori's status hint: `_meta.nori.status` on a `session_info_update`.
    pub(super) fn status(&self, status: &StatusStep) -> Run {
        let state = match status.state {
            StatusState::Working => "working",
            StatusState::Idle => "idle",
        };
        let meta = Meta::from_iter([("nori".to_owned(), json!({"status": state}))]);
        self.emit(SessionUpdate::SessionInfoUpdate(
            SessionInfoUpdate::new().meta(meta),
        ))
    }

    pub(super) fn mode(&self, mode: &ModeStep) -> Run {
        let unknown = self
            .live
            .record
            .state()
            .options
            .set(nori_acp_kit::MODE, &mode.id)
            .is_err();
        if unknown {
            // The update still goes out: announcing a mode the agent never offered is a
            // legitimate adversarial script, but it is never silent.
            self.diverge(format!(
                "mode {:?} is not one of the agent's modes",
                mode.id
            ));
        }
        self.emit(SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(
            mode.id.clone(),
        )))
    }
}
