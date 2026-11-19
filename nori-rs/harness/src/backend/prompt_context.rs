//! Keep harness observations separate from the caller's original ACP content.
use nori_protocol::acp::v1 as acp;

pub(super) fn with_context(
    original: Vec<acp::ContentBlock>,
    historical: Vec<acp::ContentBlock>,
    context: impl IntoIterator<Item = String>,
) -> Vec<acp::ContentBlock> {
    let context = context
        .into_iter()
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut content = Vec::new();
    if !context.is_empty() {
        content.push(acp::ContentBlock::Text(
            acp::TextContent::new(context)
                .annotations(acp::Annotations::new().audience(vec![acp::Role::Assistant])),
        ));
    }
    content.extend(historical);
    content.extend(original);
    content
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn injected_context_is_assistant_only_and_human_blocks_stay_byte_exact() {
        let original = vec![
            acp::ContentBlock::Text(acp::TextContent::new("  meaningful work\n")),
            acp::ContentBlock::Image(acp::ImageContent::new("aW1hZ2U=", "image/png")),
            acp::ContentBlock::Text(acp::TextContent::new("second block\n\n")),
        ];
        let historical = vec![acp::ContentBlock::Text(acp::TextContent::new(
            "archived evidence",
        ))];
        let content = with_context(
            original.clone(),
            historical.clone(),
            [
                "checkpoint".into(),
                "workflow".into(),
                "cached catalog".into(),
            ],
        );
        assert_eq!(&content[2..], original);
        assert_eq!(&content[1..2], historical);
        assert_eq!(
            content[0],
            acp::ContentBlock::Text(
                acp::TextContent::new("checkpoint\n\nworkflow\n\ncached catalog")
                    .annotations(acp::Annotations::new().audience(vec![acp::Role::Assistant]),)
            )
        );
    }

    #[test]
    fn no_context_adds_no_synthetic_block() {
        let original = vec![acp::ContentBlock::Text(acp::TextContent::new("human"))];
        assert_eq!(
            with_context(original.clone(), Vec::new(), [String::new()]),
            original
        );
    }
}
