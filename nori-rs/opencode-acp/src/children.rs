//! The OpenCode child sessions (subagents) a turn spawns, and what the client is told about them.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use nori_acp_kit::Cx;
use nori_acp_kit::acp;
use serde_json::Value;
use serde_json::json;

use crate::agent::Session;

pub(crate) struct Child {
    parent: String,
    title: Option<String>,
    depth: i64,
}

/// The turn a new session may descend from.
pub(crate) struct Parent<'a> {
    pub(crate) session: &'a Arc<Session>,
    pub(crate) id: &'a str,
    /// Whether the turn's own response is still to be sent: only then is a child of the turn's
    /// session at depth one.
    pub(crate) response_pending: bool,
    pub(crate) known_children: &'a tokio::sync::Mutex<HashMap<String, Arc<Session>>>,
}

/// Which descendants of the turn's session have been seen, and which of them still work.
#[derive(Default)]
pub(crate) struct Children {
    known: HashMap<String, Child>,
    model_known: HashMap<String, Arc<std::sync::Mutex<bool>>>,
    /// Children that have started and not yet finished: the turn is not over while any is open.
    open: HashSet<String>,
    running: HashSet<String>,
}

impl Children {
    pub(crate) fn has_open(&self) -> bool {
        !self.open.is_empty()
    }

    /// `session.created`: adopt the new session if it descends from the turn's session.
    pub(crate) async fn on_created(
        &mut self,
        root: &Parent<'_>,
        client: &Cx,
        event_session: &str,
        data: &Value,
    ) -> acp::Result<()> {
        if event_session.is_empty() || self.known.contains_key(event_session) {
            return Ok(());
        }
        let Some(parent) = data["parentID"].as_str() else {
            return Ok(());
        };
        let depth = if parent == root.id && root.response_pending {
            1
        } else if let Some(parent) = self.known.get(parent) {
            parent.depth + 1
        } else {
            return Ok(());
        };
        let title = data["title"].as_str().map(ToOwned::to_owned);
        root.known_children
            .lock()
            .await
            .insert(event_session.to_owned(), root.session.clone());
        self.known.insert(
            event_session.to_owned(),
            Child {
                parent: parent.to_owned(),
                title: title.clone(),
                depth,
            },
        );
        self.open.insert(event_session.to_owned());
        let model = model_from_event(&data["model"]);
        let model_known = Arc::new(std::sync::Mutex::new(model.is_some()));
        self.model_known
            .insert(event_session.to_owned(), model_known.clone());
        send_child(
            client,
            parent,
            event_session,
            title.as_deref(),
            model.as_deref(),
            "created",
        )?;
        // Publish the child's own controls after its identity is announced. Loading the
        // catalog is independent of the parent event stream.
        let child_id = event_session.to_owned();
        let parent_id = parent.to_owned();
        let server = root.session.server.clone();
        let child_client = client.clone();
        tokio::spawn(async move {
            let result = async {
                let child = server
                    .request(reqwest::Method::GET, &["session", &child_id], None)
                    .await
                    .map_err(|error| error.to_string())?;
                if let Some(model) = model_from_event(&child["data"]["model"]) {
                    let mut known = model_known
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if !*known {
                        send_child(
                            &child_client,
                            &parent_id,
                            &child_id,
                            None,
                            Some(&model),
                            "created",
                        )
                        .map_err(|error| error.to_string())?;
                        *known = true;
                    }
                }
                let catalog = crate::config::Catalog::load(&server, &child["data"])
                    .await
                    .map_err(|error| error.to_string())?;
                let options = catalog.options().map_err(|error| error.to_string())?;
                let event = acp::UntypedMessage::new(
                    "session/update",
                    json!({"sessionId":child_id,"update":{"sessionUpdate":"config_option_update","configOptions":options}}),
                )
                .map_err(|error| error.to_string())?;
                child_client
                    .send_notification(event)
                    .map_err(|error| error.to_string())
            }
            .await;
            if let Err(error) = result {
                tracing::warn!(%error, "Failed to load native child configuration");
            }
        });
        Ok(())
    }

    /// `session.model.selected` for a child: announce its model. Returns whether the event
    /// belonged to a child.
    pub(crate) fn on_model_selected(
        &self,
        client: &Cx,
        event_session: &str,
        data: &Value,
    ) -> acp::Result<bool> {
        let (Some(child), Some(model)) = (
            self.known.get(event_session),
            model_from_event(&data["model"]),
        ) else {
            return Ok(false);
        };
        if let Some(model_known) = self.model_known.get(event_session) {
            let mut known = model_known
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            send_child(
                client,
                &child.parent,
                event_session,
                None,
                Some(&model),
                "created",
            )?;
            *known = true;
        }
        Ok(true)
    }

    /// An execution event of a child: report its state change to the client.
    pub(crate) fn on_execution(
        &mut self,
        client: &Cx,
        event_session: &str,
        kind: &str,
    ) -> acp::Result<()> {
        let Some(child) = self.known.get(event_session) else {
            return Ok(());
        };
        let status = match kind {
            "session.execution.started" => "running",
            "session.execution.succeeded" => "completed",
            "session.execution.failed" => "failed",
            "session.execution.interrupted" => "interrupted",
            _ => return Ok(()),
        };
        let announce = if status == "running" {
            self.open.insert(event_session.to_owned());
            self.running.insert(event_session.to_owned())
        } else if self.open.remove(event_session) {
            self.running.remove(event_session);
            true
        } else {
            false
        };
        if announce {
            send_child(
                client,
                &child.parent,
                event_session,
                child.title.as_deref(),
                None,
                status,
            )?;
        }
        Ok(())
    }

    /// The child's identity as the permission request needs it: its id, parent, depth, title.
    pub(crate) fn identity(&self, id: &str) -> Option<(String, String, i64, Option<String>)> {
        self.known.get(id).map(|child| {
            (
                id.to_owned(),
                child.parent.clone(),
                child.depth,
                child.title.clone(),
            )
        })
    }
}

fn model_from_event(model: &Value) -> Option<String> {
    let provider = model["providerID"]
        .as_str()
        .filter(|value| !value.is_empty())?;
    let id = model["id"].as_str().filter(|value| !value.is_empty())?;
    Some(format!("{provider}/{id}"))
}

fn send_child(
    client: &Cx,
    parent: &str,
    child: &str,
    title: Option<&str>,
    model: Option<&str>,
    status: &str,
) -> acp::Result<()> {
    client.send_notification(acp::UntypedMessage::new(
        "session/update",
        child_update(parent, child, title, model, status)?,
    )?)
}

fn child_update(
    parent: &str,
    child: &str,
    title: Option<&str>,
    model: Option<&str>,
    status: &str,
) -> acp::Result<Value> {
    let state = match status {
        "created" => None,
        "running" => Some(json!({"state":"running"})),
        "completed" => Some(json!({"state":"idle", "stopReason":"end_turn"})),
        "failed" => Some(json!({"state":"failed"})),
        "interrupted" => Some(json!({"state":"cancelled"})),
        _ => return Err(acp::Error::internal_error().data("Unknown OpenCode child state")),
    };
    let mut update = json!({"sessionUpdate":"subagent_update", "sessionId":child});
    if let Some(state) = state {
        update["state"] = state;
    }
    if let Some(title) = title {
        update["title"] = title.into();
    }
    if let Some(model) = model {
        update["model"] = model.into();
    }
    Ok(json!({"sessionId":parent, "update":update}))
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::child_update;
    use super::model_from_event;

    #[test]
    fn uses_the_child_model_from_opencode_without_inventing_a_fallback() {
        assert_eq!(
            model_from_event(
                &json!({"providerID":"opencode-go", "id":"deepseek-v4.1-flash", "variant":"low"})
            ),
            Some("opencode-go/deepseek-v4.1-flash".to_owned())
        );
        assert_eq!(model_from_event(&json!({"id":"deepseek-v4.1-flash"})), None);
        assert_eq!(model_from_event(&json!(null)), None);
    }

    #[test]
    fn emits_the_child_model_and_keeps_it_unchanged_on_state_updates() {
        assert_eq!(
            child_update(
                "root",
                "child",
                Some("task"),
                Some("opencode-go/deepseek-v4.1-flash"),
                "created"
            )
            .unwrap(),
            json!({"sessionId":"root","update":{"sessionUpdate":"subagent_update","sessionId":"child","title":"task","model":"opencode-go/deepseek-v4.1-flash"}})
        );
        assert_eq!(
            child_update("root", "child", None, None, "running").unwrap(),
            json!({"sessionId":"root","update":{"sessionUpdate":"subagent_update","sessionId":"child","state":{"state":"running"}}})
        );
    }
}
