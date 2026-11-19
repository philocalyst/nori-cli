//! Web searches and image generation.

use crate::thread::client::SessionClient;
use crate::thread::prompt::PromptState;
use crate::thread::replay::generate_fallback_id;
use codex_protocol::models::WebSearchAction;
use codex_protocol::protocol::ImageGenerationBeginEvent;
use codex_protocol::protocol::ImageGenerationEndEvent;
use nori_acp_kit::schema::Content;
use nori_acp_kit::schema::ContentBlock;
use nori_acp_kit::schema::ImageContent;
use nori_acp_kit::schema::ResourceLink;
use nori_acp_kit::schema::TextContent;
use nori_acp_kit::schema::ToolCall;
use nori_acp_kit::schema::ToolCallContent;
use nori_acp_kit::schema::ToolCallStatus;
use nori_acp_kit::schema::ToolCallUpdate;
use nori_acp_kit::schema::ToolCallUpdateFields;
use nori_acp_kit::schema::ToolKind;

impl PromptState {
    pub(super) fn start_web_search(&mut self, client: &SessionClient, call_id: String) {
        self.active_web_search = Some(call_id.clone());
        client.send_tool_call(ToolCall::new(call_id, "Searching the Web").kind(ToolKind::Fetch));
    }

    pub(super) fn start_image_generation(
        &mut self,
        client: &SessionClient,
        event: ImageGenerationBeginEvent,
    ) {
        let raw_input = serde_json::json!(&event);
        let ImageGenerationBeginEvent { call_id } = event;
        self.active_image_generations.insert(call_id.clone());
        client.send_tool_call(
            ToolCall::new(call_id, "Image generation")
                .kind(ToolKind::Other)
                .status(ToolCallStatus::InProgress)
                .raw_input(raw_input),
        );
    }

    pub(super) fn end_image_generation(
        &mut self,
        client: &SessionClient,
        event: ImageGenerationEndEvent,
    ) {
        let raw_output = serde_json::json!(&event);
        let ImageGenerationEndEvent {
            call_id,
            status,
            revised_prompt,
            saved_path,
            ..
        } = event;
        let tool_status = image_generation_tool_status(&status);
        let saved_path = saved_path.map(|path| path.to_string_lossy().into_owned());
        // An inline image is flattened into the event as `result` (base64); one kept as a file
        // has only a `fileId`, and is shown through `saved_path`.
        let result = raw_output
            .get("result")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let content = image_generation_content(revised_prompt, result, saved_path);

        if self.active_image_generations.remove(&call_id) {
            client.send_tool_call_update(ToolCallUpdate::new(
                call_id,
                ToolCallUpdateFields::new()
                    .status(tool_status)
                    .content(content)
                    .raw_output(raw_output),
            ));
        } else {
            client.send_tool_call(
                ToolCall::new(call_id, "Image generation")
                    .kind(ToolKind::Other)
                    .status(tool_status)
                    .content(content)
                    .raw_output(raw_output),
            );
        }
    }

    pub(super) fn update_web_search_query(
        &self,
        client: &SessionClient,
        call_id: String,
        query: String,
        action: WebSearchAction,
    ) {
        let title = match &action {
            WebSearchAction::Search { query, queries } => queries
                .as_ref()
                .map(|q| format!("Searching for: {}", q.join(", ")))
                .or_else(|| query.as_ref().map(|q| format!("Searching for: {q}")))
                .unwrap_or_else(|| "Web search".to_string()),
            WebSearchAction::OpenPage { url } => url
                .as_ref()
                .map(|u| format!("Opening: {u}"))
                .unwrap_or_else(|| "Open page".to_string()),
            WebSearchAction::FindInPage { pattern, url } => match (pattern, url) {
                (Some(p), Some(u)) => format!("Finding: {p} in {u}"),
                (Some(p), None) => format!("Finding: {p}"),
                (None, Some(u)) => format!("Find in page: {u}"),
                (None, None) => "Find in page".to_string(),
            },
            WebSearchAction::Other => "Web search".to_string(),
        };

        client.send_tool_call_update(ToolCallUpdate::new(
            call_id,
            ToolCallUpdateFields::new()
                .status(ToolCallStatus::InProgress)
                .title(title)
                .raw_input(serde_json::json!({
                    "query": query,
                    "action": action
                })),
        ));
    }

    pub(super) fn complete_web_search(&mut self, client: &SessionClient) {
        if let Some(call_id) = self.active_web_search.take() {
            client.send_tool_call_update(ToolCallUpdate::new(
                call_id,
                ToolCallUpdateFields::new().status(ToolCallStatus::Completed),
            ));
        }
    }
}

/// Extract title and call_id from a WebSearchAction (used for replay)
pub(super) fn web_search_action_to_title_and_id(
    id: &Option<String>,
    action: &codex_protocol::models::WebSearchAction,
) -> (String, String) {
    match action {
        codex_protocol::models::WebSearchAction::Search { query, queries } => {
            let title = queries
                .as_ref()
                .map(|q| q.join(", "))
                .or_else(|| query.clone())
                .unwrap_or_else(|| "Web search".to_string());
            let call_id = id
                .clone()
                .unwrap_or_else(|| generate_fallback_id("web_search"));
            (title, call_id)
        }
        codex_protocol::models::WebSearchAction::OpenPage { url } => {
            let title = url.clone().unwrap_or_else(|| "Open page".to_string());
            let call_id = id
                .clone()
                .unwrap_or_else(|| generate_fallback_id("web_open"));
            (title, call_id)
        }
        codex_protocol::models::WebSearchAction::FindInPage { pattern, .. } => {
            let title = pattern
                .clone()
                .unwrap_or_else(|| "Find in page".to_string());
            let call_id = id
                .clone()
                .unwrap_or_else(|| generate_fallback_id("web_find"));
            (title, call_id)
        }
        codex_protocol::models::WebSearchAction::Other => {
            ("Unknown".to_string(), generate_fallback_id("web_search"))
        }
    }
}

pub(super) fn image_generation_tool_status(status: &str) -> ToolCallStatus {
    match status {
        "completed" => ToolCallStatus::Completed,
        "generating" | "in_progress" | "incomplete" => ToolCallStatus::InProgress,
        "failed" => ToolCallStatus::Failed,
        _ => ToolCallStatus::Completed,
    }
}

pub(super) fn image_generation_content(
    revised_prompt: Option<String>,
    result: String,
    saved_path: Option<String>,
) -> Vec<ToolCallContent> {
    let mut content = Vec::new();

    if let Some(revised_prompt) = revised_prompt.filter(|prompt| !prompt.trim().is_empty()) {
        content.push(ToolCallContent::Content(Content::new(ContentBlock::Text(
            TextContent::new(format!("Revised prompt: {revised_prompt}")),
        ))));
    }

    if !result.is_empty() {
        let mut image = ImageContent::new(result, "image/png");
        if let Some(saved_path) = saved_path
            .as_ref()
            .filter(|saved_path| !saved_path.trim().is_empty())
        {
            image = image.uri(saved_path.clone());
        }

        content.push(ToolCallContent::Content(Content::new(ContentBlock::Image(
            image,
        ))));
    } else if let Some(saved_path) = saved_path.filter(|path| !path.trim().is_empty()) {
        // Kept as a file only: point at it.
        content.push(ToolCallContent::Content(Content::new(
            ContentBlock::ResourceLink(ResourceLink::new(
                saved_path.clone(),
                format!("file://{saved_path}"),
            )),
        )));
    }

    content
}
