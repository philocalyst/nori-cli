//! The user's model choice: the routes of the chosen model, tried in order until one starts.
//!
//! Which routes exist and how they rank is the harness's (`nori_harness::recovery`). Moving a
//! session after a failure is the harness's too; the app only sees the route change.
use std::collections::VecDeque;

use super::App;
use crate::app_event::AppEvent;
use crate::nori::session_config_picker::ConfigPickerOrigin;
use crate::nori::unified_model_picker::ModelRoute;

pub(super) struct ModelRouteFallback {
    remaining: VecDeque<ModelRoute>,
    current: ModelRoute,
    name: String,
    origin: ConfigPickerOrigin,
    token: i64,
}

impl ModelRouteFallback {
    fn advance(&mut self) -> bool {
        let Some(next) = self.remaining.pop_front() else {
            return false;
        };
        self.current = next;
        true
    }
}

/// Whether a route that failed to start should give way to the next one. The user chose a model,
/// not a route, so whatever keeps one route from running it (the agent is missing, not running,
/// not signed in, unreachable, timed out, failing, out of usage, lacks the model or has no
/// capacity) says nothing about the next route, and every skip is told in the transcript. Only a
/// prompt too long for the model ends the search: every route would answer the same.
pub(crate) fn eligible_error(error: &anyhow::Error) -> bool {
    !matches!(
        nori_acp_host::classify_failure(error).class,
        nori_acp_host::FailureClass::PromptTooLong
    )
}

pub(super) fn eligible_acp_error(error: &nori_protocol::acp::v1::Error) -> bool {
    serde_json::to_value(error)
        .ok()
        .and_then(|value| serde_json::from_value::<nori_protocol::acp::v1::Error>(value).ok())
        .is_some_and(|error| eligible_error(&anyhow::Error::new(error)))
}

/// How the transcript names a route.
fn route_label(route: &ModelRoute) -> String {
    match route {
        ModelRoute::Session { .. } => "The agent in use".to_owned(),
        ModelRoute::Agent { display_name, .. } => display_name.clone(),
    }
}

impl App {
    pub(super) fn stale_model_route_token(&self, token: Option<i64>) -> bool {
        token.is_some_and(|token| {
            self.model_route_fallback
                .as_ref()
                .is_none_or(|route| route.token != token)
        })
    }

    pub(super) async fn select_model_routes(
        &mut self,
        routes: Vec<ModelRoute>,
        name: String,
        origin: ConfigPickerOrigin,
    ) {
        if self.model_route_fallback.is_some() || self.manual_model_handoff.is_some() {
            self.discard_candidate();
        }
        self.manual_model_handoff = None;
        let mut remaining = VecDeque::from(routes);
        let Some(current) = remaining.pop_front() else {
            self.model_route_fallback = None;
            self.app_event_tx.send(AppEvent::ShowToast(
                "No available route for this model".to_owned(),
            ));
            return;
        };
        self.next_model_route_token = self.next_model_route_token.saturating_add(1);
        self.model_route_fallback = Some(ModelRouteFallback {
            remaining,
            current,
            name,
            origin,
            token: self.next_model_route_token,
        });
        self.apply_model_route().await;
    }

    async fn apply_model_route(&mut self) {
        let Some(fallback) = self.model_route_fallback.as_ref() else {
            return;
        };
        match &fallback.current {
            ModelRoute::Session {
                config_id,
                value,
                option_name,
                ..
            } => {
                self.app_event_tx.send(AppEvent::SetAcpSessionConfigOption {
                    config_id: config_id.clone(),
                    value: value.clone(),
                    option_name: option_name.clone(),
                    value_name: fallback.name.clone(),
                    is_custom_model: false,
                    origin: fallback.origin,
                    route_token: Some(fallback.token),
                });
            }
            ModelRoute::Agent {
                agent_name,
                display_name,
                model,
            } => {
                let (agent, display, model) =
                    (agent_name.clone(), display_name.clone(), model.clone());
                self.select_agent_model(agent, display, model).await;
            }
        }
    }

    /// The route the model choice is on failed because of `why`: say so in the transcript and
    /// try the next route, or, when none is left, end the choice with the reason. `true` when
    /// another route is being tried; `false` also when no model choice was in progress.
    pub(super) async fn advance_model_route(&mut self, why: &str) -> bool {
        let Some(fallback) = self.model_route_fallback.as_mut() else {
            return false;
        };
        let failed = route_label(&fallback.current);
        let name = fallback.name.clone();
        if !fallback.advance() {
            self.end_model_choice(why);
            return false;
        }
        let next = route_label(&fallback.current);
        self.chat_widget.add_info_message(
            format!("{failed} couldn't run {name}: {why}"),
            Some(format!("trying {next}")),
        );
        self.apply_model_route().await;
        true
    }

    /// The model choice failed because of `why` and goes no further: say so, once, and end it.
    /// Nothing happens when no choice is in progress (or it was already ended).
    pub(super) fn end_model_choice(&mut self, why: &str) {
        if let Some(fallback) = self.model_route_fallback.take() {
            self.chat_widget.add_error_message(format!(
                "Couldn't switch to {}: {}: {why}",
                fallback.name,
                route_label(&fallback.current)
            ));
        }
    }

    pub(super) fn is_model_session_route(
        &self,
        token: Option<i64>,
        config_id: &str,
        value: &str,
    ) -> bool {
        self.model_route_fallback.as_ref().is_some_and(|route| token == Some(route.token) && route.origin == ConfigPickerOrigin::Direct && matches!(&route.current, ModelRoute::Session { config_id: id, value: selected, .. } if id == config_id && selected == value))
    }

    pub(super) fn is_model_child_route(
        &self,
        token: Option<i64>,
        key: crate::subagent_switcher::SubagentKey,
        config_id: &str,
        value: &str,
    ) -> bool {
        self.model_route_fallback.as_ref().is_some_and(|route| token == Some(route.token) && route.origin == ConfigPickerOrigin::Child(key) && matches!(&route.current, ModelRoute::Session { config_id: id, value: selected, .. } if id == config_id && selected == value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn route_retry_preserves_same_model_choice() {
        let first = ModelRoute::Agent {
            agent_name: "a".into(),
            display_name: "A".into(),
            model: "gpt-6.1-terra".into(),
        };
        let second = ModelRoute::Agent {
            agent_name: "b".into(),
            display_name: "B".into(),
            model: "gateway/gpt-6.1-terra".into(),
        };
        let mut fallback = ModelRouteFallback {
            remaining: VecDeque::from([second.clone()]),
            current: first,
            name: "GPT 6.1 Terra".into(),
            origin: ConfigPickerOrigin::Direct,
            token: 1,
        };
        assert!(fallback.advance());
        assert_eq!(fallback.current, second);
        assert_eq!(fallback.name, "GPT 6.1 Terra");
        assert!(!fallback.advance());
    }

    #[test]
    fn start_failures_trigger_route_retry() {
        let auth = anyhow::Error::new(nori_protocol::acp::v1::Error::auth_required());
        assert!(eligible_error(&auth));
        let quota = anyhow::Error::new(
            nori_protocol::acp::v1::Error::internal_error()
                .data(serde_json::json!({"code":"insufficient_quota"})),
        );
        assert!(eligible_error(&quota));
        let quota = anyhow::Error::new(
            nori_protocol::acp::v1::Error::new(-32003, "Mock provider quota exhausted")
                .data(serde_json::json!({"code":"insufficient_quota"})),
        )
        .context("Failed to set ACP session config option");
        assert!(eligible_error(&quota));
        assert!(eligible_error(&anyhow::anyhow!("error sending request")));
    }
}
