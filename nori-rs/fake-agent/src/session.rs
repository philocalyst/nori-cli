//! A fake agent's sessions. The record outlives the connection, so `session/load` and
//! `session/resume` on a new connection find the conversation where the last one left it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;

use nori_acp_kit::Options;
use serde_json::Value;

/// One prompt and every `session/update` the agent sent for it, as JSON, for replay at load.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TurnRecord {
    pub prompt: String,
    pub updates: Vec<Value>,
}

#[derive(Debug)]
pub struct SessionState {
    pub title: Option<String>,
    pub options: Options,
    /// Prompts received so far; the next prompt's index.
    pub prompts: i64,
    pub history: Vec<TurnRecord>,
    /// Values host tool results and the script bound.
    pub vars: BTreeMap<String, Value>,
    /// Calls announced, for generating ids.
    pub tool_calls: i64,
    pub subagents: i64,
}

/// A session of one fake agent.
#[derive(Debug)]
pub struct SessionRecord {
    pub id: String,
    pub agent: String,
    /// Order of creation across the world.
    pub number: i64,
    pub cwd: PathBuf,
    pub created_at: i64,
    state: Mutex<SessionState>,
}

impl SessionRecord {
    pub fn new(
        id: String,
        agent: String,
        number: i64,
        cwd: PathBuf,
        created_at: i64,
        options: Options,
    ) -> Self {
        Self {
            id,
            agent,
            number,
            cwd,
            created_at,
            state: Mutex::new(SessionState {
                title: None,
                options,
                prompts: 0,
                history: Vec::new(),
                vars: BTreeMap::new(),
                tool_calls: 0,
                subagents: 0,
            }),
        }
    }

    pub fn state(&self) -> MutexGuard<'_, SessionState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn model(&self) -> Option<String> {
        self.state()
            .options
            .current(nori_acp_kit::MODEL)
            .map(str::to_owned)
    }

    /// A copy under a new id, for `session/fork`.
    pub fn forked(&self, id: String, number: i64, created_at: i64) -> Self {
        let state = self.state();
        Self {
            id,
            agent: self.agent.clone(),
            number,
            cwd: self.cwd.clone(),
            created_at,
            state: Mutex::new(SessionState {
                title: state.title.clone(),
                options: state.options.clone(),
                prompts: state.prompts,
                history: state.history.clone(),
                vars: state.vars.clone(),
                tool_calls: state.tool_calls,
                subagents: state.subagents,
            }),
        }
    }
}
