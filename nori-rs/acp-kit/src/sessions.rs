//! The table of sessions an agent holds.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use crate::error::AgentError;
use crate::schema::SessionId;

/// Sessions by id. Lookups hand out `Arc`s, so a session outlives its removal for as long as a
/// task still works on it.
#[derive(Debug)]
pub struct Sessions<S> {
    table: Mutex<HashMap<SessionId, Arc<S>>>,
}

impl<S> Default for Sessions<S> {
    fn default() -> Self {
        Self {
            table: Mutex::new(HashMap::new()),
        }
    }
}

impl<S> Sessions<S> {
    fn table(&self) -> std::sync::MutexGuard<'_, HashMap<SessionId, Arc<S>>> {
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn insert(&self, id: SessionId, session: S) -> Arc<S> {
        let session = Arc::new(session);
        self.table().insert(id, Arc::clone(&session));
        session
    }

    pub fn get(&self, id: &SessionId) -> Option<Arc<S>> {
        self.table().get(id).cloned()
    }

    /// The session `id`, or the error that tells the client it does not exist.
    pub fn require(&self, id: &SessionId) -> Result<Arc<S>, AgentError> {
        self.get(id).ok_or_else(|| AgentError::unknown_session(id))
    }

    pub fn remove(&self, id: &SessionId) -> Option<Arc<S>> {
        self.table().remove(id)
    }

    /// Every session, leaving the table empty.
    pub fn drain(&self) -> Vec<Arc<S>> {
        self.table().drain().map(|(_, session)| session).collect()
    }

    pub fn snapshot(&self) -> Vec<(SessionId, Arc<S>)> {
        self.table()
            .iter()
            .map(|(id, session)| (id.clone(), Arc::clone(session)))
            .collect()
    }
}
