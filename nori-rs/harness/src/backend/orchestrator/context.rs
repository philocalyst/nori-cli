//! Bounded, read-only access to the canonical conversation projection.
use crate::transcript::fork_context::Keep;
use nori_conversation::Record;
use serde_json::Value;
use serde_json::json;

impl super::Orchestrator {
    pub(crate) async fn conversation_context(
        &self,
        start: Option<i64>,
        end: Option<i64>,
        limit: Option<i64>,
    ) -> anyhow::Result<Value> {
        let backend = self
            .inner
            .program_backend
            .get()
            .and_then(std::sync::Weak::upgrade)
            .ok_or_else(|| anyhow::anyhow!("conversation runtime is no longer available"))?;
        let recorder = match &self.inner.caller_view {
            Some(view) => backend.native_children.history(view).await?,
            None => backend
                .transcript_recorder_cell()
                .read()
                .await
                .clone()
                .ok_or_else(|| anyhow::anyhow!("this session has no conversation journal"))?,
        };
        let records = recorder.visible_records().await?;
        let agent_budget = crate::context_budget::ContextBudget::for_agent(backend.agent_name());
        context_page(
            recorder.session_id(),
            &records,
            start,
            end,
            limit,
            agent_budget.page_bytes(),
        )
    }
}

fn context_page(
    conversation_id: &str,
    records: &[Record],
    start: Option<i64>,
    end: Option<i64>,
    limit: Option<i64>,
    page_bytes: usize,
) -> anyhow::Result<Value> {
    let limit = limit.unwrap_or(50);
    anyhow::ensure!(
        (1..=200).contains(&limit),
        "limit must be between 1 and 200"
    );
    let limit = usize::try_from(limit)?;
    let through = match end {
        Some(id) => {
            records
                .iter()
                .position(|record| record.id == id)
                .ok_or_else(|| anyhow::anyhow!("end is not visible in this conversation"))?
                + 1
        }
        None => records.len(),
    };
    let from = match start {
        Some(id) => records
            .iter()
            .position(|record| record.id == id)
            .ok_or_else(|| anyhow::anyhow!("start is not visible in this conversation"))?,
        None => through.saturating_sub(limit),
    };
    anyhow::ensure!(start.is_none() || from < through, "start must precede end");
    let page_end = from.saturating_add(limit).min(through);
    let selected = &records[from..page_end];
    // A page is also bounded in bytes, as it becomes part of the reader's own context. One read
    // from the head keeps the newest records; one read from `start` keeps the oldest and
    // continues at `next_start`, so a page cut short by size loses nothing to a later read.
    let keep = if start.is_some() {
        Keep::Oldest
    } else {
        Keep::Newest
    };
    let digest = crate::transcript::fork_context::page_from_records(selected, page_bytes, keep);
    let (selected_start, selected_end) = digest.included.unzip();
    let next_start = match keep {
        Keep::Oldest => digest
            .next_id
            .or_else(|| (page_end < through).then(|| records[page_end].id)),
        Keep::Newest => digest
            .next_id
            .or_else(|| (from > 0).then(|| records[from - 1].id)),
    };
    Ok(json!({
        "conversation_id": conversation_id,
        "head": records.last().map(|record| record.id),
        "visible_start": records.first().map(|record| record.id),
        "visible_end": records.last().map(|record| record.id),
        "selected_start": selected_start,
        "selected_end": selected_end,
        "next_start": next_start,
        "truncated": digest.omitted > 0,
        "context": digest.json,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nori_conversation::Event;
    use pretty_assertions::assert_eq;

    const PAGE: usize = 64 * 1024;

    /// The log's visible records, as the context page reads them.
    fn visible(log: &nori_conversation::ConversationLog) -> Vec<Record> {
        log.visible()
            .map(|record| record.map(std::sync::Arc::unwrap_or_clone))
            .collect::<std::io::Result<_>>()
            .unwrap()
    }

    fn records() -> Vec<Record> {
        [1, 3, 8, 12]
            .into_iter()
            .map(|id| Record {
                id,
                command_id: None,
                event: Event::Message {
                    role: "user".into(),
                    content: format!("message {id}"),
                },
            })
            .collect()
    }

    #[test]
    fn defaults_to_recent_records_and_pages_by_visible_ids() {
        let records = records();
        let recent = context_page("test", &records, None, None, Some(2), PAGE).unwrap();
        assert_eq!(recent["selected_start"], 8);
        assert_eq!(recent["selected_end"], 12);
        assert_eq!(recent["next_start"], 3);
        let page = context_page("test", &records, Some(1), Some(12), Some(2), PAGE).unwrap();
        assert_eq!(page["head"], 12);
        assert_eq!(page["next_start"], 8);
        let context = page["context"].as_str().unwrap();
        assert!(context.contains("message 1"));
        assert!(!context.contains("message 8"));
    }

    #[test]
    fn a_size_truncated_head_page_returns_the_first_older_record_to_read() {
        let records: Vec<Record> = (1..=10)
            .map(|id| Record {
                id,
                command_id: None,
                event: Event::Message {
                    role: "user".into(),
                    content: "x".repeat(8 * 1024),
                },
            })
            .collect();

        let page = context_page("test", &records, None, None, None, 2 * 1024).unwrap();
        assert_eq!(page["truncated"], true);
        let next_start = page["next_start"].as_i64().unwrap();
        let selected_start = page["selected_start"].as_i64().unwrap();
        assert_eq!(next_start + 1, selected_start);
    }

    #[test]
    fn journal_reads_use_the_rewound_projection_and_leave_storage_untouched() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("conversation.pkl");
        let mut log = nori_conversation::ConversationLog::open(&path).unwrap();
        let first = log
            .append(Event::Message {
                role: "user".into(),
                content: "retained".into(),
            })
            .unwrap();
        let hidden = log
            .append(Event::Command {
                name: "unsafe".into(),
                payload: "never execute this".into(),
            })
            .unwrap();
        log.append(Event::Rewind {
            head: Some(first.id),
        })
        .unwrap();
        log.append(Event::Message {
            role: "user".into(),
            content: "new branch".into(),
        })
        .unwrap();
        let before = std::fs::read(&path).unwrap();
        let reopened = nori_conversation::ConversationLog::open(&path).unwrap();
        let page = context_page("test", &visible(&reopened), None, None, None, PAGE).unwrap();
        let context = page["context"].as_str().unwrap();
        assert!(context.contains("retained"));
        assert!(context.contains("new branch"));
        assert!(!context.contains("never execute this"));
        assert!(
            context_page(
                "test",
                &visible(&reopened),
                Some(hidden.id),
                None,
                None,
                PAGE
            )
            .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(visible(&log), visible(&reopened));
    }

    #[test]
    fn rejects_hidden_ids_reversed_ranges_and_invalid_limits() {
        for (start, end, limit) in [
            (Some(2), None, None),
            (None, Some(2), None),
            (Some(12), Some(1), None),
            (Some(12), Some(8), None),
            (None, None, Some(0)),
            (None, None, Some(201)),
        ] {
            assert!(context_page("test", &records(), start, end, limit, PAGE).is_err());
        }
        let empty = context_page("test", &[], None, None, None, PAGE).unwrap();
        assert_eq!(empty["head"], Value::Null);
        assert_eq!(empty["selected_start"], Value::Null);
    }
}
