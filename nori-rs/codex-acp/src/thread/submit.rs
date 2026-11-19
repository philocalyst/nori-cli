//! Turning an ACP prompt into a Codex submission: slash commands, user input items, and the
//! per-submission state that follows the turn's events.

use crate::thread::actor::ThreadActor;
use crate::thread::backends::Auth;
use crate::thread::prompt::PromptState;
use crate::thread::prompt::SubmissionState;
use codex_core::review_prompts::user_facing_hint;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ReviewRequest;
use codex_protocol::protocol::ReviewTarget;
use codex_protocol::turn_input::TurnInputMode;
use codex_protocol::turn_input::TurnInputSubmission;
use codex_protocol::user_input::UserInput;
use nori_acp_kit::acp::Error;
use nori_acp_kit::schema::ContentBlock;
use nori_acp_kit::schema::EmbeddedResource;
use nori_acp_kit::schema::EmbeddedResourceResource;
use nori_acp_kit::schema::PromptRequest;
use nori_acp_kit::schema::ResourceLink;
use nori_acp_kit::schema::StopReason;
use nori_acp_kit::schema::TextResourceContents;
use tokio::sync::oneshot;
use tracing::info;
use tracing::warn;

pub(super) const INIT_COMMAND_PROMPT: &str = include_str!("../prompt_for_init_command.md");

impl<A: Auth> ThreadActor<A> {
    pub(super) async fn handle_prompt(
        &mut self,
        request: PromptRequest,
    ) -> Result<oneshot::Receiver<Result<StopReason, Error>>, Error> {
        let (response_tx, response_rx) = oneshot::channel();

        let items = build_prompt_items(request.prompt);
        let mut turn_replies = Vec::new();
        let op;
        if let Some((name, rest)) = extract_slash_command(&items) {
            match name {
                "compact" => op = Op::Compact,
                "init" => {
                    op = user_turn_op(
                        vec![UserInput::Text {
                            text: INIT_COMMAND_PROMPT.into(),
                            text_elements: vec![],
                        }],
                        &mut turn_replies,
                    )
                }
                "review" => {
                    let instructions = rest.trim();
                    let target = if instructions.is_empty() {
                        ReviewTarget::UncommittedChanges
                    } else {
                        ReviewTarget::Custom {
                            instructions: instructions.to_owned(),
                        }
                    };

                    op = Op::Review {
                        review_request: ReviewRequest {
                            user_facing_hint: Some(user_facing_hint(&target)),
                            target,
                        },
                    }
                }
                "review-branch" if !rest.is_empty() => {
                    let target = ReviewTarget::BaseBranch {
                        branch: rest.trim().to_owned(),
                    };
                    op = Op::Review {
                        review_request: ReviewRequest {
                            user_facing_hint: Some(user_facing_hint(&target)),
                            target,
                        },
                    }
                }
                "review-commit" if !rest.is_empty() => {
                    let target = ReviewTarget::Commit {
                        sha: rest.trim().to_owned(),
                        title: None,
                    };
                    op = Op::Review {
                        review_request: ReviewRequest {
                            user_facing_hint: Some(user_facing_hint(&target)),
                            target,
                        },
                    }
                }
                "logout" => {
                    self.auth.logout().await?;
                    return Err(Error::auth_required());
                }
                _ => op = user_turn_op(items, &mut turn_replies),
            }
        } else {
            op = user_turn_op(items, &mut turn_replies)
        }

        let submission_id = self
            .thread
            .submit(op)
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?;

        // Core says whether the input started a turn or was refused; only a refusal is news.
        for reply in turn_replies {
            tokio::spawn(async move {
                if let Ok(Err(error)) = reply.await {
                    warn!("Turn input was not accepted: {error}");
                }
            });
        }

        info!("Submitted prompt with submission_id: {submission_id}");
        info!("Starting to wait for conversation events for submission_id: {submission_id}");

        let mut prompt = PromptState::new(
            submission_id.clone(),
            self.thread.clone(),
            self.resolution_tx.clone(),
            response_tx,
        );
        prompt.host_tools = self.host_tools.clone();
        prompt.model = Some(self.get_current_model().await);
        let state = SubmissionState::Prompt(prompt);

        self.submissions.insert(submission_id, state);

        Ok(response_rx)
    }
}

pub(super) fn build_prompt_items(prompt: Vec<ContentBlock>) -> Vec<UserInput> {
    prompt
        .into_iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text_block) => Some(UserInput::Text {
                text: text_block.text,
                text_elements: vec![],
            }),
            ContentBlock::Image(image_block) => Some(UserInput::Image {
                image: codex_protocol::models::ImageReference::Inline {
                    image_url: format!(
                        "data:{};base64,{}",
                        image_block.mime_type, image_block.data
                    ),
                },
                detail: None,
            }),
            ContentBlock::ResourceLink(ResourceLink { name, uri, .. }) => Some(UserInput::Text {
                text: format_uri_as_link(Some(name), uri),
                text_elements: vec![],
            }),
            ContentBlock::Resource(EmbeddedResource {
                resource:
                    EmbeddedResourceResource::TextResourceContents(TextResourceContents {
                        text,
                        uri,
                        ..
                    }),
                ..
            }) => Some(UserInput::Text {
                text: format!(
                    "{}\n<context ref=\"{uri}\">\n{text}\n</context>",
                    format_uri_as_link(None, uri.clone())
                ),
                text_elements: vec![],
            }),
            // Skip other content types for now
            ContentBlock::Audio(..) | ContentBlock::Resource(..) | _ => None,
        })
        .collect()
}

pub(super) fn format_uri_as_link(name: Option<String>, uri: String) -> String {
    if let Some(name) = name
        && !name.is_empty()
    {
        format!("[@{name}]({uri})")
    } else if let Some(path) = uri.strip_prefix("file://") {
        let name = path.split('/').next_back().unwrap_or(path);
        format!("[@{name}]({uri})")
    } else if uri.starts_with("zed://") {
        let name = uri.split('/').next_back().unwrap_or(&uri);
        format!("[@{name}]({uri})")
    } else {
        uri
    }
}

/// The op that submits `items` as a user turn: start one when idle, otherwise steer the active
/// one. The receiver is Core's answer, which the caller watches.
pub(super) fn user_turn_op(
    items: Vec<UserInput>,
    replies: &mut Vec<oneshot::Receiver<codex_protocol::error::Result<TurnInputSubmission>>>,
) -> Op {
    let (reply, receiver) = oneshot::channel();
    replies.push(receiver);
    Op::TurnInput {
        request: Box::new(codex_core_api::TurnInputRequest::user_input(items)),
        mode: TurnInputMode::StartOrSteer,
        reply,
    }
}

/// Checks if a prompt is slash command
pub(super) fn extract_slash_command(content: &[UserInput]) -> Option<(&str, &str)> {
    let line = content.first().and_then(|block| match block {
        UserInput::Text { text, .. } => Some(text),
        _ => None,
    })?;
    // Parse a first-line slash command of the form `/name <rest>`.
    // Returns `(name, rest_after_name)` if the line begins with `/` and contains
    // a non-empty name; otherwise returns `None`.
    let stripped = line.strip_prefix('/')?;
    let mut name_end = stripped.len();
    for (idx, ch) in stripped.char_indices() {
        if ch.is_whitespace() {
            name_end = idx;
            break;
        }
    }
    let name = &stripped[..name_end];
    if name.is_empty() {
        return None;
    }
    let rest = stripped[name_end..].trim_start();
    Some((name, rest))
}
