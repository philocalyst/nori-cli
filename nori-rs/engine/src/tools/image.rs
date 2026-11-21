//! `view_image`: Codex's image viewer, a local image handed to the model as an image.

use std::path::Path;

use async_trait::async_trait;
use base64::Engine;
use nori_acp_kit::schema::ToolKind;
use nori_llm::ToolResultPart;
use nori_llm::ToolSpec;
use serde_json::Value;
use serde_json::json;

use super::Invocation;
use super::Tool;
use super::ToolContext;
use super::ToolOutput;
use super::blocking;
use super::paths;
use super::string_arg;
use crate::flavor::ModelInfo;

/// The largest image handed to the model.
const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;

pub struct ViewImage {
    pub name: String,
    pub description: String,
}

impl Default for ViewImage {
    fn default() -> Self {
        Self {
            name: "view_image".to_owned(),
            description: "View a local image file from the filesystem when visual inspection is needed. Use this for images already available on disk.".to_owned(),
        }
    }
}

fn media_type(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    })
}

fn load(path: &Path) -> Result<ToolOutput, String> {
    let media_type = media_type(path).ok_or_else(|| {
        format!(
            "{} is not an image this tool reads (png, jpeg, gif, webp).",
            path.display()
        )
    })?;
    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("Cannot read {}: {error}", path.display()))?;
    if metadata.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "{} is {} MB, too large to view.",
            path.display(),
            metadata.len() / (1024 * 1024)
        ));
    }
    let bytes =
        std::fs::read(path).map_err(|error| format!("Cannot read {}: {error}", path.display()))?;
    Ok(ToolOutput {
        result: vec![ToolResultPart::Image {
            media_type: media_type.to_owned(),
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        }],
        ..ToolOutput::default()
    }
    .display(vec![format!("Viewed {}", path.display()).into()]))
}

#[async_trait]
impl Tool for ViewImage {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: self.description.clone(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Local filesystem path to an image file."},
                },
                "required": ["path"],
                "additionalProperties": false,
            }),
        }
    }

    async fn prepare(&self, input: &Value, context: &ToolContext) -> Result<Invocation, String> {
        let path = paths::resolve(string_arg(input, "path")?, context);
        let mut invocation = Invocation::new(
            format!("View {}", paths::display(&path, context)),
            ToolKind::Read,
        );
        invocation.checks = paths::checks("read", &path, context);
        invocation.locations = vec![path];
        Ok(invocation)
    }

    async fn run(&self, input: Value, context: &ToolContext) -> ToolOutput {
        let Ok(raw) = string_arg(&input, "path") else {
            return ToolOutput::error("The `path` argument is required.");
        };
        if !context.images {
            return ToolOutput::error("The current model does not read images.");
        }
        let path = paths::resolve(raw, context);
        match blocking(move || load(&path)).await {
            Ok(Ok(output)) => output,
            Ok(Err(error)) | Err(error) => ToolOutput::error(error),
        }
    }

    fn offered(&self, model: &ModelInfo) -> bool {
        model.images
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[tokio::test]
    async fn an_image_reaches_the_model_and_a_text_file_does_not() {
        let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
        std::fs::write(directory.path().join("pixel.png"), [137, 80, 78, 71])
            .unwrap_or_else(|error| panic!("{error}"));
        std::fs::write(directory.path().join("notes.txt"), "text")
            .unwrap_or_else(|error| panic!("{error}"));
        let context = ToolContext::new(
            directory.path().to_path_buf(),
            vec![directory.path().to_path_buf()],
            nori_tools::WorkspaceConfig::default(),
        );
        let tool = ViewImage::default();
        let output = tool.run(json!({"path": "pixel.png"}), &context).await;
        assert_eq!(
            output.result,
            vec![ToolResultPart::Image {
                media_type: "image/png".to_owned(),
                data: "iVBORw==".to_owned(),
            }]
        );
        assert!(
            tool.run(json!({"path": "notes.txt"}), &context)
                .await
                .is_error
        );
    }
}
