//! What the model is told: the system prompt, and a person's prompt as a message.

use std::path::Path;

use nori_acp_kit::AgentError;
use nori_acp_kit::schema::ContentBlock;
use nori_acp_kit::schema::EmbeddedResourceResource;
use nori_acp_kit::schema::Role as AcpRole;
use nori_llm::Message;
use nori_llm::Part;

use crate::flavor::CommandDef;
use crate::flavor::Mode;
use crate::flavor::Workspace;

/// The largest file a mention attaches whole.
const MAX_ATTACHED_BYTES: u64 = 256 * 1024;

/// The system prompt, in parts: the stable ones first, so a provider can cache them.
pub(crate) fn system(
    base: String,
    mode: Option<&Mode>,
    workspace: &Workspace,
    cwd: &Path,
    host_instructions: Option<String>,
) -> Vec<String> {
    let mut parts = vec![base];
    if let Some(prompt) = mode.and_then(|mode| mode.prompt.clone()) {
        parts.push(prompt);
    }
    let is_git = cwd
        .ancestors()
        .any(|directory| directory.join(".git").exists());
    let today = nori_acp_kit::rfc3339_utc(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(0)),
    );
    parts.push(format!(
        "<env>\nWorking directory: {}\nIs the directory a git repository: {}\nPlatform: {}\nToday's date: {}\n</env>",
        cwd.display(),
        if is_git { "yes" } else { "no" },
        std::env::consts::OS,
        today.get(..10).unwrap_or(&today),
    ));
    for instruction in &workspace.instructions {
        parts.push(format!(
            "Instructions from {}:\n{}",
            instruction.source.display(),
            instruction.text
        ));
    }
    if let Some(instructions) = host_instructions {
        parts.push(instructions);
    }
    parts
}

/// A prompt as the user message the model reads. A slash command naming one of `commands`
/// becomes its expanded template.
pub(crate) async fn user_message(
    blocks: Vec<ContentBlock>,
    commands: &[CommandDef],
) -> Result<Message, AgentError> {
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(text) => {
                let audience = text
                    .annotations
                    .as_ref()
                    .and_then(|annotations| annotations.audience.as_ref());
                if audience.is_some_and(|audience| !audience.contains(&AcpRole::Assistant)) {
                    continue;
                }
                parts.push(Part::text(text.text));
            }
            ContentBlock::Image(image) if !image.data.is_empty() => parts.push(Part::Image {
                media_type: image.mime_type,
                data: image.data,
            }),
            ContentBlock::Image(image) => {
                if let Some(uri) = image.uri {
                    parts.push(Part::text(format!("[image]({uri})")));
                }
            }
            ContentBlock::Resource(resource) => match resource.resource {
                EmbeddedResourceResource::TextResourceContents(text) => parts.push(Part::text(
                    format!("<file uri=\"{}\">\n{}\n</file>", text.uri, text.text),
                )),
                EmbeddedResourceResource::BlobResourceContents(blob) => {
                    match blob.mime_type.filter(|mime| mime.starts_with("image/")) {
                        Some(media_type) => parts.push(Part::Image {
                            media_type,
                            data: blob.blob,
                        }),
                        None => parts.push(Part::text(format!("[attachment]({})", blob.uri))),
                    }
                }
                _ => return Err(AgentError::invalid("Unsupported embedded resource")),
            },
            ContentBlock::ResourceLink(link) => {
                parts.push(Part::text(attach_link(&link.uri, &link.name).await));
            }
            ContentBlock::Audio(_) => {}
            _ => return Err(AgentError::invalid("Unsupported prompt content")),
        }
    }
    expand_command(&mut parts, commands);
    Ok(Message::user(parts))
}

/// A mentioned file's content, or the link as text when it is not a readable local file.
async fn attach_link(uri: &str, name: &str) -> String {
    let Some(path) = uri.strip_prefix("file://").map(ToOwned::to_owned) else {
        return format!("[{name}]({uri})");
    };
    let read = tokio::task::spawn_blocking(move || -> Option<String> {
        let metadata = std::fs::metadata(&path).ok()?;
        if !metadata.is_file() || metadata.len() > MAX_ATTACHED_BYTES {
            return None;
        }
        let text = std::fs::read_to_string(&path).ok()?;
        Some(format!("<file path=\"{path}\">\n{text}\n</file>"))
    })
    .await
    .ok()
    .flatten();
    read.unwrap_or_else(|| format!("@{}", uri.trim_start_matches("file://")))
}

fn expand_command(parts: &mut [Part], commands: &[CommandDef]) {
    let Some(Part::Text { text }) = parts.first_mut() else {
        return;
    };
    let Some(rest) = text.strip_prefix('/') else {
        return;
    };
    let (name, arguments) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    if let Some(command) = commands.iter().find(|command| command.name == name) {
        *text = command.expand(arguments.trim());
    }
}
