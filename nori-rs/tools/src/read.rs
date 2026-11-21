//! The `read` tool: numbered lines of a file, a directory listing, or an image.

use std::path::Path;

use crate::error::ToolError;
use crate::fs::FileKind;
use crate::fs::FileStat;
use crate::io::looks_binary;
use crate::ledger::LineRange;
use crate::ledger::hash_bytes;
use crate::repair::ReadArgs;
use crate::result::ImageAttachment;
use crate::result::ToolResult;
use crate::util::cpu;
use crate::workspace::Workspace;

fn image_mime(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_string_lossy().to_lowercase();
    match extension.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

fn is_pdf(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
}

/// Cut `line` to `max` characters, saying how much was dropped.
fn truncate_line(line: &str, max: usize) -> (String, bool) {
    let count = line.chars().count();
    if count <= max {
        return (line.to_owned(), false);
    }
    let head: String = line.chars().take(max).collect();
    (format!("{head}… [{} more characters]", count - max), true)
}

impl Workspace {
    pub(crate) async fn read(&self, args: ReadArgs) -> Result<ToolResult, ToolError> {
        let path = self.resolve_path(&args.file_path);
        let stat = self.stat_existing(&path).await?;
        let config = self.config();
        let limit = args
            .limit
            .filter(|limit| *limit > 0)
            .unwrap_or(config.read_line_limit)
            .min(config.read_line_limit);
        let offset = args.offset.unwrap_or(1).max(1);

        if stat.kind == FileKind::Dir {
            return self.list_dir(&path, offset, limit).await;
        }
        if let Some(mime) = image_mime(&path) {
            return self.read_image(&path, &stat, mime).await;
        }
        if is_pdf(&path) {
            return Ok(ToolResult::error(format!(
                "{} is a PDF, which `read` cannot show. Extract its text with a shell command \
                 such as `pdftotext file.pdf -`.",
                path.display()
            )));
        }
        if stat.len > config.max_file_bytes {
            return Err(ToolError::TooLarge {
                path,
                bytes: stat.len,
                limit: config.max_file_bytes,
            });
        }

        let bytes = self
            .fs()
            .read(&path)
            .await
            .map_err(|error| ToolError::io("read", &path, error))?;
        if looks_binary(&bytes) {
            return Err(ToolError::Binary {
                path,
                bytes: i64::try_from(bytes.len()).unwrap_or(i64::MAX),
            });
        }
        let size = bytes.len();
        let (hash, text, lossy) = cpu(size, move || {
            let hash = hash_bytes(&bytes);
            match String::from_utf8(bytes) {
                Ok(text) => (hash, text, false),
                Err(error) => (
                    hash,
                    String::from_utf8_lossy(error.as_bytes()).into_owned(),
                    true,
                ),
            }
        })
        .await?;

        let text = text.strip_prefix('\u{FEFF}').unwrap_or(&text);
        let mut lines: Vec<&str> = text.split('\n').collect();
        if text.ends_with('\n') || text.is_empty() {
            lines.pop();
        }
        let total = i64::try_from(lines.len()).unwrap_or(i64::MAX);
        if total == 0 {
            self.ledger()
                .record_read(&path, &hash, stat.modified, 0, None);
            return Ok(ToolResult::ok(format!(
                "{} is empty (0 lines). Nothing to read; use `write` or `edit` with an empty \
                 old_string to add content.",
                path.display()
            )));
        }
        if offset > total {
            return Err(ToolError::OffsetPastEnd {
                path,
                offset,
                total_lines: total,
            });
        }

        let first = usize::try_from(offset - 1).unwrap_or(0);
        let wanted = usize::try_from(limit).unwrap_or(usize::MAX);
        let max_chars = usize::try_from(config.max_read_output_chars).unwrap_or(usize::MAX);
        let max_line = usize::try_from(config.max_line_chars).unwrap_or(usize::MAX);
        let mut out = String::new();
        let mut shown = 0usize;
        let mut cut_lines = 0;
        for (index, line) in lines.iter().enumerate().skip(first).take(wanted) {
            let line = line.strip_suffix('\r').unwrap_or(line);
            let (line, cut) = truncate_line(line, max_line);
            if cut {
                cut_lines += 1;
            }
            let rendered = format!("{:>6}→{line}\n", index + 1);
            if shown > 0 && out.len() + rendered.len() > max_chars {
                break;
            }
            out.push_str(&rendered);
            shown += 1;
        }
        let last = first + shown;
        let last_line = i64::try_from(last).unwrap_or(i64::MAX);
        self.ledger().record_read(
            &path,
            &hash,
            stat.modified,
            total,
            Some(LineRange {
                start: offset,
                end: last_line,
            }),
        );

        let mut notes = Vec::new();
        if last_line < total {
            notes.push(format!(
                "Showing lines {offset}-{last_line} of {total}. Call `read` again with \
                 offset={} to continue.",
                last_line + 1
            ));
        } else if offset > 1 {
            notes.push(format!("End of file: {total} lines."));
        }
        if cut_lines > 0 {
            notes.push(format!(
                "{cut_lines} very long line(s) were shortened; use `grep` to find what you need \
                 in them."
            ));
        }
        if lossy {
            notes.push(
                "This file is not valid UTF-8; invalid bytes are shown as �. It cannot be edited \
                 with these tools."
                    .to_owned(),
            );
        }
        if !notes.is_empty() {
            out.push_str(&format!("[{}]", notes.join(" ")));
        } else {
            out.pop();
        }
        Ok(ToolResult::ok(out))
    }

    async fn list_dir(
        &self,
        path: &Path,
        offset: i64,
        limit: i64,
    ) -> Result<ToolResult, ToolError> {
        let entries = self
            .fs()
            .read_dir(path)
            .await
            .map_err(|error| ToolError::io("list", path, error))?;
        let total = i64::try_from(entries.len()).unwrap_or(i64::MAX);
        if entries.is_empty() {
            return Ok(ToolResult::ok(format!(
                "{} is an empty directory.",
                path.display()
            )));
        }
        if offset > total {
            return Err(ToolError::OffsetPastEnd {
                path: path.to_path_buf(),
                offset,
                total_lines: total,
            });
        }
        let first = usize::try_from(offset - 1).unwrap_or(0);
        let wanted = usize::try_from(limit).unwrap_or(usize::MAX);
        let mut lines: Vec<String> = entries
            .iter()
            .skip(first)
            .take(wanted)
            .map(|entry| match entry.kind {
                FileKind::Dir => format!("{}/", entry.name),
                FileKind::File | FileKind::Other => entry.name.clone(),
            })
            .collect();
        let last = offset - 1 + i64::try_from(lines.len()).unwrap_or(0);
        if last < total {
            lines.push(format!(
                "[Showing entries {offset}-{last} of {total}. Call `read` again with offset={}.]",
                last + 1
            ));
        }
        Ok(ToolResult::ok(format!(
            "Directory {}:\n{}",
            path.display(),
            lines.join("\n")
        )))
    }

    async fn read_image(
        &self,
        path: &Path,
        stat: &FileStat,
        mime: &str,
    ) -> Result<ToolResult, ToolError> {
        let limit = self.config().max_image_bytes;
        if stat.len > limit {
            return Err(ToolError::TooLarge {
                path: path.to_path_buf(),
                bytes: stat.len,
                limit,
            });
        }
        let data = self
            .fs()
            .read(path)
            .await
            .map_err(|error| ToolError::io("read", path, error))?;
        let mut result = ToolResult::ok(format!(
            "Image {} ({mime}, {} bytes) is attached.",
            path.display(),
            data.len()
        ));
        result.images.push(ImageAttachment {
            mime_type: mime.to_owned(),
            data,
        });
        Ok(result)
    }
}
