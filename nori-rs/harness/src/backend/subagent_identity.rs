//! Attribute native child announcements to the ACP connection that owns them.

pub(super) fn native_child(
    mut wire: nori_protocol::SubagentWire,
    agent: &str,
) -> nori_protocol::SubagentWire {
    if let nori_protocol::SubagentWire::Update(update) = &mut wire {
        update.agent = nori_protocol::Patch::Set(agent.to_owned());
    }
    wire
}

#[cfg(test)]
mod tests {
    use super::*;
    use nori_protocol::Patch;
    use nori_protocol::SubagentUpdate;
    use nori_protocol::SubagentWire;
    use nori_protocol::acp::v1::SessionId;
    use pretty_assertions::assert_eq;

    #[test]
    fn native_child_uses_owning_connection_provider() {
        let update = SubagentUpdate {
            parent_session_id: SessionId::new("parent"),
            session_id: SessionId::new("child"),
            agent: Patch::Keep,
            model: Patch::Keep,
            name: Patch::Keep,
            title: Patch::Set("misleading title".to_owned()),
            description: Patch::Keep,
            state: Patch::Keep,
        };
        let mut expected = update.clone();
        expected.agent = Patch::Set("claude-code".to_owned());
        assert_eq!(
            native_child(SubagentWire::Update(update), "claude-code"),
            SubagentWire::Update(expected),
        );
    }
}
