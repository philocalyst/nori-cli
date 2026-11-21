//! The shared file tools (`nori-tools`: `read`, `write`, `edit`, `multi_edit`, `apply_patch`) as
//! engine tools.
//!
//! One [`SharedTool`] per catalog entry. The tools themselves run in the session's
//! `nori_tools::Workspace` (see [`ToolContext::files`]); this adapter only says what a call is
//! about (title, ACP kind, the paths it touches and the permission checks they need), and maps
//! the result: text and images for the model, changed files as ACP diffs for the client.

use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use nori_acp_kit::schema::Diff;
use nori_acp_kit::schema::ToolCallContent;
use nori_acp_kit::schema::ToolKind;
use nori_llm::ToolResultPart;
use nori_llm::ToolSpec;
use nori_tools::EditStyle;
use nori_tools::PermissionAction;
use nori_tools::ToolKind as FileTool;
use serde_json::Value;

use super::Check;
use super::Invocation;
use super::Tool;
use super::ToolContext;
use super::ToolOutput;
use super::paths;

/// One of `nori-tools`' file tools, offered to the model under its catalog name.
pub struct SharedTool {
    kind: FileTool,
    spec: nori_tools::ToolSpec,
}

impl SharedTool {
    /// The file tools `style` offers, to hand a flavor's `tools()`.
    pub fn catalog(style: EditStyle) -> Vec<Arc<dyn Tool>> {
        nori_tools::catalog(style)
            .into_iter()
            .filter_map(|spec| {
                let kind = FileTool::resolve(spec.name)?;
                Some(Arc::new(Self { kind, spec }) as Arc<dyn Tool>)
            })
            .collect()
    }
}

#[async_trait]
impl Tool for SharedTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.spec.name.to_owned(),
            description: self.spec.description.clone(),
            input_schema: self.spec.input_schema.clone(),
        }
    }

    async fn prepare(&self, input: &Value, context: &ToolContext) -> Result<Invocation, String> {
        let preview = context
            .files
            .preview(self.spec.name, input.clone())
            .await
            .map_err(|error| nori_tools::ToolResult::from_error(&error).text)?;
        let (verb, kind, action) = match (self.kind, preview.action) {
            (_, PermissionAction::Read) => ("Read", ToolKind::Read, "read"),
            (FileTool::Write, PermissionAction::Edit) => ("Write", ToolKind::Edit, "edit"),
            (_, PermissionAction::Edit) => ("Edit", ToolKind::Edit, "edit"),
        };
        let title = match preview.paths.as_slice() {
            [one] => format!("{verb} {}", paths::display(&one.path, context)),
            many => format!("{verb} {} files", many.len()),
        };
        let mut invocation = Invocation::new(title, kind);
        for target in &preview.paths {
            if target.external {
                let directory = target.path.parent().unwrap_or(&target.path);
                invocation.checks.push(Check::new(
                    "external_directory",
                    directory.to_string_lossy().into_owned(),
                ));
            }
            invocation
                .checks
                .push(Check::new(action, paths::display(&target.path, context)));
        }
        invocation.preview = preview
            .changes
            .into_iter()
            .map(|change| {
                ToolCallContent::Diff(
                    Diff::new(change.path, change.new_text).old_text(change.old_text),
                )
            })
            .collect();
        invocation.locations = preview
            .paths
            .into_iter()
            .map(|target| target.path)
            .collect();
        Ok(invocation)
    }

    fn handles_cancellation(&self) -> bool {
        true
    }

    async fn run(&self, input: Value, context: &ToolContext) -> ToolOutput {
        let result = context
            .files
            .call_with_cancel(self.spec.name, input, context.cancel.clone())
            .await;
        let mut parts = vec![ToolResultPart::Text { text: result.text }];
        if !result.images.is_empty() {
            if !context.images {
                return ToolOutput::error(
                    "That is an image, and the current model does not read images.",
                );
            }
            parts.extend(
                result
                    .images
                    .into_iter()
                    .map(|image| ToolResultPart::Image {
                        media_type: image.mime_type,
                        data: base64::engine::general_purpose::STANDARD.encode(image.data),
                    }),
            );
        }
        let display = result
            .changes
            .into_iter()
            .map(|change| {
                ToolCallContent::Diff(
                    Diff::new(change.path, change.new_text.unwrap_or_default())
                        .old_text(change.old_text),
                )
            })
            .collect();
        ToolOutput {
            result: parts,
            is_error: result.is_error,
            display,
            ..ToolOutput::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;

    fn context(directory: &std::path::Path) -> ToolContext {
        ToolContext::new(
            directory.to_path_buf(),
            vec![directory.to_path_buf()],
            nori_tools::WorkspaceConfig::default(),
        )
    }

    fn tool(style: EditStyle, name: &str) -> Arc<dyn Tool> {
        SharedTool::catalog(style)
            .into_iter()
            .find(|tool| tool.spec().name == name)
            .unwrap_or_else(|| panic!("no tool {name}"))
    }

    #[test]
    fn each_style_offers_its_catalog_in_order() {
        let names = |style| {
            SharedTool::catalog(style)
                .iter()
                .map(|tool| tool.spec().name)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(EditStyle::Replace),
            ["read", "write", "edit", "multi_edit"]
        );
        assert_eq!(names(EditStyle::Patch), ["read", "apply_patch"]);
    }

    #[tokio::test]
    async fn a_patch_is_asked_about_per_file_including_outside_the_roots() {
        let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
        let context = context(directory.path());
        let patch = "*** Begin Patch\n*** Add File: b.txt\n+y\n*** Add File: /elsewhere/c.txt\n+z\n*** End Patch";
        let invocation = tool(EditStyle::Patch, "apply_patch")
            .prepare(&json!({"input": patch}), &context)
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(invocation.title, "Edit 2 files");
        assert_eq!(invocation.kind, ToolKind::Edit);
        let checks: Vec<(String, String)> = invocation
            .checks
            .into_iter()
            .map(|check| (check.action, check.resource))
            .collect();
        assert_eq!(
            checks,
            [
                ("external_directory".to_owned(), "/elsewhere".to_owned()),
                ("edit".to_owned(), "/elsewhere/c.txt".to_owned()),
                ("edit".to_owned(), "b.txt".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn an_edit_is_previewed_as_a_diff_without_touching_the_file() {
        let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
        let file = directory.path().join("a.txt");
        std::fs::write(&file, "one\ntwo\n").unwrap_or_else(|error| panic!("{error}"));
        let context = context(directory.path());
        let read = tool(EditStyle::Replace, "read")
            .run(json!({"file_path": "a.txt"}), &context)
            .await;
        assert!(!read.is_error);
        let invocation = tool(EditStyle::Replace, "edit")
            .prepare(
                &json!({"file_path": "a.txt", "old_string": "two", "new_string": "2"}),
                &context,
            )
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        let checks: Vec<(String, String)> = invocation
            .checks
            .into_iter()
            .map(|check| (check.action, check.resource))
            .collect();
        assert_eq!(checks, [("edit".to_owned(), "a.txt".to_owned())]);
        assert_eq!(
            invocation.preview,
            [ToolCallContent::Diff(
                Diff::new(file.clone(), "one\n2\n").old_text("one\ntwo\n".to_owned())
            )]
        );
        assert_eq!(
            std::fs::read_to_string(&file).unwrap_or_default(),
            "one\ntwo\n"
        );
    }

    #[tokio::test]
    async fn an_edit_needs_a_read_first_then_reports_its_diff() {
        let directory = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
        std::fs::write(directory.path().join("a.txt"), "one\ntwo\n")
            .unwrap_or_else(|error| panic!("{error}"));
        let context = context(directory.path());
        let edit = tool(EditStyle::Replace, "edit");
        let args = json!({"file_path": "a.txt", "old_string": "two", "new_string": "2"});
        assert!(edit.run(args.clone(), &context).await.is_error);
        let read = tool(EditStyle::Replace, "read")
            .run(json!({"file_path": "a.txt"}), &context)
            .await;
        assert!(!read.is_error);
        let output = edit.run(args, &context).await;
        assert!(!output.is_error);
        assert_eq!(output.display.len(), 1);
        assert_eq!(
            std::fs::read_to_string(directory.path().join("a.txt")).unwrap_or_default(),
            "one\n2\n"
        );
    }
}
