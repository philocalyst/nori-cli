//! One open session: its conversation, dials, tools, granted permissions and journal.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;

use nori_acp_kit::HostBridge;
use nori_acp_kit::TurnGate;
use nori_acp_kit::TurnPolicy;
use nori_llm::Message;

use crate::flavor::Mode;
use crate::flavor::Workspace;
use crate::mcp::McpTools;
use crate::options::Dials;
use crate::rules::Rule;
use crate::store::Journal;
use crate::tools::HostTool;
use crate::tools::Tool;
use crate::tools::ToolContext;

/// A message waiting for the loop, and the ticket of the prompt that sent it.
pub(crate) struct Queued {
    pub(crate) ticket: i64,
    pub(crate) message: Message,
}

pub(crate) struct Session {
    pub(crate) id: String,
    pub(crate) cwd: PathBuf,
    /// A prompt that arrives while a turn runs joins it at the next step.
    pub(crate) gate: TurnGate,
    /// Prompts the loop has not read yet.
    inbox: Mutex<Vec<Queued>>,
    tickets: AtomicI64,
    /// The conversation the model reads. Held by the one loop that runs, for the whole turn.
    pub(crate) conversation: tokio::sync::Mutex<Vec<Message>>,
    /// Read at every step, so a change made mid-turn applies at the next model call.
    dials: Mutex<Dials>,
    /// Replaced when the flavor's workspace changes under the session (a newer model catalog).
    workspace: Mutex<Arc<Workspace>>,
    pub(crate) journal: Journal,
    pub(crate) host: Option<HostBridge>,
    /// The tools by name.
    pub(crate) tools: HashMap<String, Arc<dyn Tool>>,
    /// Their order, as the model sees them.
    pub(crate) tool_order: Vec<String>,
    /// What the session's tools share; each turn gets a copy with its own cancellation.
    pub(crate) tool_context: ToolContext,
    /// The client's MCP servers' tools, as they become ready.
    mcp: Arc<McpTools>,
    /// Rules added by "always allow" answers.
    granted: Mutex<Vec<Rule>>,
}

pub(crate) struct Parts {
    pub(crate) id: String,
    pub(crate) cwd: PathBuf,
    pub(crate) roots: Vec<PathBuf>,
    pub(crate) workspace: Arc<Workspace>,
    pub(crate) dials: Dials,
    pub(crate) journal: Journal,
    pub(crate) host: Option<HostBridge>,
    pub(crate) history: Vec<Message>,
    pub(crate) flavor_tools: Vec<Arc<dyn Tool>>,
    /// How the file tools behave in this session.
    pub(crate) files: nori_tools::WorkspaceConfig,
    pub(crate) mcp_servers: Vec<nori_acp_kit::schema::McpServer>,
}

impl Session {
    pub(crate) fn new(parts: Parts) -> Self {
        let host_tools = parts.host.as_ref().map(HostTool::all).unwrap_or_default();
        let host_names: Vec<String> = host_tools.iter().map(|tool| tool.spec().name).collect();
        let mut tools = HashMap::new();
        let mut tool_order = Vec::new();
        for tool in parts.flavor_tools.into_iter().chain(host_tools) {
            if tool
                .replaced_by_host()
                .is_some_and(|host| host_names.iter().any(|name| name == host))
            {
                continue;
            }
            let name = tool.spec().name;
            if tools.insert(name.clone(), tool).is_none() {
                tool_order.push(name);
            }
        }
        Self {
            id: parts.id,
            cwd: parts.cwd.clone(),
            gate: TurnGate::new(TurnPolicy::Steerable),
            inbox: Mutex::new(Vec::new()),
            tickets: AtomicI64::new(0),
            conversation: tokio::sync::Mutex::new(parts.history),
            dials: Mutex::new(parts.dials),
            workspace: Mutex::new(parts.workspace),
            journal: parts.journal,
            host: parts.host,
            tools,
            tool_order,
            mcp: McpTools::start(parts.mcp_servers, parts.cwd.clone()),
            tool_context: ToolContext::new(parts.cwd, parts.roots, parts.files),
            granted: Mutex::new(Vec::new()),
        }
    }

    /// The tool called `name`: the session's own, else an MCP server's.
    pub(crate) fn tool(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned().or_else(|| {
            self.mcp
                .tools()
                .into_iter()
                .find(|tool| tool.spec().name == name)
        })
    }

    /// Every tool ready now, in the order the model sees them.
    pub(crate) fn tool_list(&self) -> Vec<Arc<dyn Tool>> {
        self.tool_order
            .iter()
            .filter_map(|name| self.tools.get(name).cloned())
            .chain(self.mcp.tools())
            .collect()
    }

    /// Queue a prompt's message for the loop; the ticket says whether a loop read it.
    pub(crate) fn enqueue(&self, message: Message) -> i64 {
        let ticket = self.tickets.fetch_add(1, Ordering::SeqCst);
        self.inbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Queued { ticket, message });
        ticket
    }

    /// Whether the prompt with `ticket` is still waiting for a loop.
    pub(crate) fn waiting(&self, ticket: i64) -> bool {
        self.inbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .any(|queued| queued.ticket == ticket)
    }

    pub(crate) fn has_waiting(&self) -> bool {
        !self
            .inbox
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_empty()
    }

    /// Everything waiting, in order, now read.
    pub(crate) fn take_waiting(&self) -> Vec<Message> {
        std::mem::take(&mut *self.inbox.lock().unwrap_or_else(PoisonError::into_inner))
            .into_iter()
            .map(|queued| queued.message)
            .collect()
    }

    pub(crate) fn dials(&self) -> Dials {
        self.dials
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Change a dial; returns the dials as they now stand.
    pub(crate) fn set_dial(
        &self,
        id: &str,
        value: &str,
    ) -> Result<Dials, nori_acp_kit::AgentError> {
        let mut dials = self.dials.lock().unwrap_or_else(PoisonError::into_inner);
        dials.set(&self.workspace(), id, value)?;
        Ok(dials.clone())
    }

    pub(crate) fn mode(&self, dials: &Dials) -> Option<Mode> {
        let id = dials.mode.as_ref()?;
        self.workspace()
            .modes
            .iter()
            .find(|mode| &mode.id == id)
            .cloned()
    }

    /// What the session offers now.
    pub(crate) fn workspace(&self) -> Arc<Workspace> {
        Arc::clone(
            &self
                .workspace
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }

    /// Offer `workspace` from now on; returns the dials as they then stand.
    pub(crate) fn replace_workspace(&self, workspace: Arc<Workspace>) -> Dials {
        *self
            .workspace
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = workspace;
        self.dials()
    }

    /// The rules in force: the workspace's, the mode's, then the person's answers.
    pub(crate) fn rules(&self, dials: &Dials) -> Vec<Rule> {
        let mut rules = self.workspace().rules.clone();
        if let Some(mode) = self.mode(dials) {
            rules.extend(mode.rules.iter().cloned());
        }
        rules.extend(
            self.granted
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .cloned(),
        );
        rules
    }

    pub(crate) fn grant(&self, rules: Vec<Rule>) {
        self.granted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(rules);
    }
}
