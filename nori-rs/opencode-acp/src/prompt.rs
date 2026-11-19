//! An ACP prompt as OpenCode takes it: text, attached files, and context that is not the user's.

use nori_acp_kit::AgentError;
use nori_acp_kit::schema::ContentBlock;
use nori_acp_kit::schema::EmbeddedResourceResource;
use nori_acp_kit::schema::Role;
use serde_json::Value;
use serde_json::json;

pub(crate) struct PromptInput {
    /// What the user typed, joined.
    pub(crate) text: String,
    /// Attachments as OpenCode's `files` array.
    pub(crate) files: Vec<Value>,
    /// Context annotated for the assistant only: delivered as a synthetic message.
    pub(crate) synthetic: Vec<String>,
}

impl PromptInput {
    pub(crate) fn from_blocks(blocks: Vec<ContentBlock>) -> Result<Self, AgentError> {
        let mut text = Vec::new();
        let mut files = Vec::new();
        let mut synthetic = Vec::new();
        for block in blocks {
            match block {
                ContentBlock::Text(part) => {
                    let audience = part
                        .annotations
                        .as_ref()
                        .and_then(|annotations| annotations.audience.as_ref());
                    if audience.is_some_and(|audience| !audience.contains(&Role::Assistant)) {
                        continue;
                    }
                    if audience.is_some_and(|audience| !audience.contains(&Role::User)) {
                        synthetic.push(part.text);
                    } else {
                        text.push(part.text);
                    }
                }
                ContentBlock::Image(part) => {
                    let uri = if part.data.is_empty() {
                        part.uri
                            .ok_or_else(|| AgentError::invalid("Image has no data or URI"))?
                    } else {
                        format!("data:{};base64,{}", part.mime_type, part.data)
                    };
                    if uri.starts_with("data:") || uri.starts_with("file://") {
                        files.push(json!({"uri":uri,"name":"image"}));
                    } else {
                        text.push(format!("[image]({uri})"));
                    }
                }
                ContentBlock::Resource(part) => match part.resource {
                    EmbeddedResourceResource::TextResourceContents(resource) => {
                        text.push(format!("[{}]\n{}", resource.uri, resource.text));
                    }
                    EmbeddedResourceResource::BlobResourceContents(resource) => {
                        if let Some(mime) = resource.mime_type {
                            let uri = if resource.uri.starts_with("data:") {
                                resource.uri
                            } else {
                                format!("data:{mime};base64,{}", resource.blob)
                            };
                            files.push(json!({"uri":uri,"name":"file"}));
                        }
                    }
                    _ => return Err(AgentError::invalid("Unsupported embedded resource")),
                },
                ContentBlock::ResourceLink(part) if part.uri.starts_with("file://") => {
                    files.push(json!({"uri":part.uri,"name":part.name}));
                }
                ContentBlock::ResourceLink(part) => {
                    text.push(format!("[{}]({})", part.name, part.uri));
                }
                ContentBlock::Audio(_) => {}
                _ => return Err(AgentError::invalid("Unsupported prompt content")),
            }
        }
        Ok(Self {
            text: text.join("\n"),
            files,
            synthetic,
        })
    }

    /// The slash command the text starts with: its name and arguments.
    pub(crate) fn slash_command(&self) -> Option<(String, String)> {
        let text = self.text.strip_prefix('/')?;
        let (name, args) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
        (!name.is_empty()).then(|| (name.to_owned(), args.trim_start().to_owned()))
    }
}
