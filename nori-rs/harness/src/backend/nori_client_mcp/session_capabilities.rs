//! The capability snapshot a session reports about its agent and the `nori-client` surface.

use super::*;

pub(in crate::backend) fn capabilities_update_for_session(
    connection: &AcpConnection,
    connected: &AtomicBool,
    goal_tools_available: bool,
) -> crate::normalized::SessionCapabilitiesView {
    capabilities_update_for_nori_client(
        connection,
        reachable(connection) || super::host_tools::accepted_by(connection),
        connected.load(Ordering::Relaxed),
        goal_tools_available,
    )
}

pub(in crate::backend) fn capabilities_update_for_nori_client(
    connection: &AcpConnection,
    nori_client_advertised: bool,
    nori_client_initialized: bool,
    goal_tools_available: bool,
) -> crate::normalized::SessionCapabilitiesView {
    let goal_ext_advertised =
        goal_ext::GoalExtCapability::from_initialize_meta(connection.initialize_meta()).is_some();
    capabilities_update(
        agent_capabilities_view(connection),
        nori_client_advertised,
        nori_client_initialized,
        goal_ext_advertised,
        goal_tools_available,
    )
}

/// Project the agent's ACP capabilities into the client-facing view.
pub(crate) fn agent_capabilities_view(
    connection: &AcpConnection,
) -> crate::normalized::AgentCapabilitiesView {
    let capabilities = connection.capabilities();
    crate::normalized::AgentCapabilitiesView {
        http_mcp: capabilities.mcp_capabilities.http,
        load_session: capabilities.load_session,
        session_list: capabilities.session_capabilities.list.is_some(),
        session_resume: capabilities.session_capabilities.resume.is_some(),
        session_close: capabilities.session_capabilities.close.is_some(),
        session_fork: capabilities.session_capabilities.fork.is_some(),
    }
}

pub(super) fn capabilities_update(
    agent: crate::normalized::AgentCapabilitiesView,
    nori_client_advertised: bool,
    nori_client_initialized: bool,
    goal_ext_advertised: bool,
    goal_tools_available: bool,
) -> crate::normalized::SessionCapabilitiesView {
    let goal_available = goal_tools_available || goal_ext_advertised;
    crate::normalized::SessionCapabilitiesView {
        agent,
        nori_client: crate::normalized::NoriClientCapabilitiesView {
            advertised: nori_client_advertised,
            initialized: nori_client_initialized,
        },
        builtin_commands: HashMap::from([(
            "goal".to_string(),
            nori_protocol::CommandAvailability {
                enabled: goal_available,
                reason: (!goal_available).then(|| {
                    if nori_client_advertised {
                        "Nori goal tools are disabled by session policy.".to_owned()
                    } else {
                        "The active agent advertises neither HTTP MCP support nor the goal extension, so /goal is unavailable.".to_owned()
                    }
                }),
            },
        )]),
    }
}
