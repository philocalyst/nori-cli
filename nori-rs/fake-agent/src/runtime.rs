//! The state every agent of a world shares: the clock, the flight recorder, counters, the
//! sessions that outlive a connection, and the process-wide table that finds a world by name.
//!
//! A world is shared because delegation crosses connections: a parent's `delegate_task` starts
//! another connection, on another fake agent of the same world, and they must agree on the time,
//! on what has been recorded, and on how many sessions exist.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;

use crate::clock::Clock;
use crate::clock::ClockMode;
use crate::record::FlightRecorder;
use crate::session::SessionRecord;
use crate::world::World;
use crate::world::WorldError;

/// A world that is running: its script, its clock, and what has happened in it.
pub struct WorldRuntime {
    pub world: World,
    pub clock: Clock,
    pub recorder: FlightRecorder,
    counters: Mutex<HashMap<String, i64>>,
    sessions: Mutex<HashMap<String, Arc<SessionRecord>>>,
}

impl std::fmt::Debug for WorldRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorldRuntime")
            .field("world", &self.world.name)
            .field("clock", &self.clock)
            .finish_non_exhaustive()
    }
}

static BY_ID: LazyLock<Mutex<HashMap<String, Arc<WorldRuntime>>>> = LazyLock::new(Mutex::default);
static BY_PATH: LazyLock<Mutex<HashMap<String, Arc<WorldRuntime>>>> = LazyLock::new(Mutex::default);
static NEXT_ID: AtomicI64 = AtomicI64::new(1);

/// The prefix of a spec's `world=` value that names a world installed in this process.
const INSTALLED_PREFIX: &str = "mem:";

impl WorldRuntime {
    /// A fresh runtime for `world`: new clock, empty recorder, no sessions.
    pub fn new(world: World, mode: ClockMode) -> Arc<Self> {
        let start = crate::clock::parse_rfc3339_utc(&world.start).unwrap_or_default();
        Arc::new(Self {
            world,
            clock: Clock::new(mode, start),
            recorder: FlightRecorder::default(),
            counters: Mutex::default(),
            sessions: Mutex::default(),
        })
    }

    /// Make this runtime findable by the `world=` option a spec carries, until
    /// [`WorldRuntime::uninstall`]. Returns the option's value.
    pub fn install(self: &Arc<Self>) -> String {
        let id = format!(
            "{}{}",
            INSTALLED_PREFIX,
            NEXT_ID.fetch_add(1, Ordering::SeqCst)
        );
        BY_ID
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id.clone(), Arc::clone(self));
        id
    }

    pub fn uninstall(reference: &str) {
        BY_ID
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(reference);
    }

    /// The runtime a spec's `world=` names: an installed world (`mem:N`), or a world file,
    /// loaded once per process and mode so every connection of the process shares it.
    pub fn resolve(reference: &str, mode: ClockMode) -> Result<Arc<Self>, WorldError> {
        if reference.starts_with(INSTALLED_PREFIX) {
            return BY_ID
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(reference)
                .cloned()
                .ok_or_else(|| WorldError::Invalid {
                    origin: reference.to_owned(),
                    message: "no such world is installed in this process (was it dropped?)"
                        .to_owned(),
                });
        }
        let path = Path::new(reference);
        let key = format!(
            "{}|{mode:?}",
            path.canonicalize()
                .unwrap_or_else(|_| path.to_path_buf())
                .display()
        );
        if let Some(runtime) = BY_PATH
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
        {
            return Ok(Arc::clone(runtime));
        }
        let runtime = Self::new(World::from_path(path)?, mode);
        BY_PATH
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, Arc::clone(&runtime));
        Ok(runtime)
    }

    /// The next value of counter `key`, starting at 0.
    pub fn next(&self, key: &str) -> i64 {
        let mut counters = self.counters.lock().unwrap_or_else(PoisonError::into_inner);
        let counter = counters.entry(key.to_owned()).or_insert(0);
        let value = *counter;
        *counter += 1;
        value
    }

    pub fn session(&self, id: &str) -> Option<Arc<SessionRecord>> {
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(id)
            .cloned()
    }

    pub fn insert_session(&self, session: SessionRecord) -> Arc<SessionRecord> {
        let session = Arc::new(session);
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(session.id.clone(), Arc::clone(&session));
        session
    }

    pub fn remove_session(&self, id: &str) -> Option<Arc<SessionRecord>> {
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id)
    }

    /// The sessions of `agent`, oldest first.
    pub fn sessions_of(&self, agent: &str) -> Vec<Arc<SessionRecord>> {
        let mut sessions: Vec<_> = self
            .sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .filter(|session| session.agent == agent)
            .cloned()
            .collect();
        sessions.sort_by_key(|session| session.number);
        sessions
    }
}
