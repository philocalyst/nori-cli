//! Separate human input from assistant-only harness context in mock behaviors.
use nori_protocol::acp::v1 as acp;

pub(crate) fn user_text(block: &acp::ContentBlock) -> Option<&str> {
    let acp::ContentBlock::Text(text) = block else {
        return None;
    };
    let audience = text
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.audience.as_ref());
    audience
        .is_none_or(|audience| audience.contains(&acp::Role::User))
        .then_some(text.text.as_str())
}

/// Wire traces retain all text and typed content, including assistant-only context.
pub(crate) fn model_text(block: &acp::ContentBlock) -> Option<&str> {
    match block {
        acp::ContentBlock::Text(text) => Some(text.text.as_str()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn assistant_context_is_model_visible_but_never_part_of_human_echoes() {
        let context = acp::ContentBlock::Text(
            acp::TextContent::new("catalog")
                .annotations(acp::Annotations::new().audience(vec![acp::Role::Assistant])),
        );
        let human = acp::ContentBlock::Text(acp::TextContent::new("  meaningful work\n"));
        assert_eq!(user_text(&context), None);
        assert_eq!(model_text(&context), Some("catalog"));
        assert_eq!(user_text(&human), Some("  meaningful work\n"));
        assert_eq!(model_text(&human), Some("  meaningful work\n"));
    }
}
