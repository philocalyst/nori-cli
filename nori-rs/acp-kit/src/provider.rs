//! The short road to an agent: describe the provider, let the kit do the rest.
//!
//! A [`Provider`] has sessions that are plain values. It opens one, runs a turn on it, and
//! may interrupt it, configure it and close it. [`ProviderAgent`] turns that into a full
//! [`NativeAgent`]: the session table, one prompt at a time per session, cancellation,
//! `session/close`, `set_config_option` and shutdown are the kit's. A harness-harness (an agent
//! whose job is to drive someone else's engine and translate its events) is the `Provider`
//! implementation and nothing else.
//!
//! ```ignore
//! struct Echo;
//!
//! #[async_trait]
//! impl Provider for Echo {
//!     type Session = ();
//!     fn profile(&self) -> Profile {
//!         Profile::new("nori-echo-acp", "Echo", "0.1")
//!     }
//!     async fn open(&self, _request: NewSessionRequest) -> Result<Opened<()>, AgentError> {
//!         Ok(Opened::new(SessionId::new("echo-1"), ()))
//!     }
//!     async fn run_turn(&self, _: &(), input: Vec<ContentBlock>, turn: &TurnCtx)
//!         -> Result<StopReason, AgentError>
//!     {
//!         turn.sink.text(format!("{} blocks", input.len()))?;
//!         Ok(StopReason::EndTurn)
//!     }
//! }
//!
//! pub fn registration() -> Registration {
//!     Registration::new("echo", |_spec| async { Ok(ProviderAgent::new(Echo)) })
//! }
//! ```

use agent_client_protocol as acp;
use async_trait::async_trait;

use crate::agent::NativeAgent;
use crate::agent::Profile;
use crate::error::AgentError;
use crate::schema::CloseSessionRequest;
use crate::schema::CloseSessionResponse;
use crate::schema::ContentBlock;
use crate::schema::NewSessionRequest;
use crate::schema::NewSessionResponse;
use crate::schema::PromptRequest;
use crate::schema::PromptResponse;
use crate::schema::SessionConfigOption;
use crate::schema::SessionId;
use crate::schema::SetSessionConfigOptionRequest;
use crate::schema::SetSessionConfigOptionResponse;
use crate::schema::StopReason;
use crate::sessions::Sessions;
use crate::sink::Cx;
use crate::sink::Sink;
use crate::turn::Turn;
use crate::turn::TurnGate;
use crate::turn::TurnPolicy;

/// What opening a session produced.
#[derive(Debug)]
pub struct Opened<S> {
    pub id: SessionId,
    pub session: S,
    /// The session's configuration options, as `session/new` returns them.
    pub options: Vec<SessionConfigOption>,
}

impl<S> Opened<S> {
    pub fn new(id: SessionId, session: S) -> Self {
        Self {
            id,
            session,
            options: Vec::new(),
        }
    }

    pub fn options(mut self, options: Vec<SessionConfigOption>) -> Self {
        self.options = options;
        self
    }
}

/// A running turn, as the provider sees it: where to send updates and whether to stop.
#[derive(Debug)]
pub struct TurnCtx {
    pub sink: Sink,
    turn: Turn,
}

impl TurnCtx {
    pub fn is_cancelled(&self) -> bool {
        self.turn.is_cancelled()
    }

    /// Completes when the client cancels the turn.
    pub async fn cancelled(&self) {
        self.turn.cancelled().await;
    }

    pub fn token(&self) -> tokio_util::sync::CancellationToken {
        self.turn.token()
    }
}

/// An engine the kit can serve as an ACP agent.
#[async_trait]
pub trait Provider: Send + Sync + 'static {
    /// One session's state.
    type Session: Send + Sync + 'static;

    /// Identity and capabilities. `close` is always on, the kit provides it.
    fn profile(&self) -> Profile;

    /// What a prompt that arrives while a turn runs does. Refused unless the engine says otherwise.
    fn turn_policy(&self) -> TurnPolicy {
        TurnPolicy::Exclusive
    }

    async fn open(&self, request: NewSessionRequest) -> Result<Opened<Self::Session>, AgentError>;

    /// Run one prompt. Updates go out through `turn.sink`; the return value is why the turn
    /// ended. A turn that was cancelled is reported as cancelled whatever this returns.
    async fn run_turn(
        &self,
        session: &Self::Session,
        input: Vec<ContentBlock>,
        turn: &TurnCtx,
    ) -> Result<StopReason, AgentError>;

    /// The running turn was cancelled: tell the engine to stop.
    async fn interrupt(&self, _session: &Self::Session) {}

    /// Apply a configuration option and return the options as they now stand.
    async fn configure(
        &self,
        _session: &Self::Session,
        _id: &str,
        _value: &str,
    ) -> Result<Vec<SessionConfigOption>, AgentError> {
        Err(AgentError::Unsupported("session/set_config_option"))
    }

    /// The session is closed or the connection ended: stop what it started.
    async fn close(&self, _session: &Self::Session) {}
}

struct Slot<S> {
    gate: TurnGate,
    session: S,
}

/// A [`Provider`] served as a [`NativeAgent`].
pub struct ProviderAgent<P: Provider> {
    provider: P,
    sessions: Sessions<Slot<P::Session>>,
}

impl<P: Provider> ProviderAgent<P> {
    pub fn new(provider: P) -> Self {
        Self {
            provider,
            sessions: Sessions::default(),
        }
    }
}

#[async_trait]
impl<P: Provider> NativeAgent for ProviderAgent<P> {
    fn profile(&self) -> Profile {
        let mut profile = self.provider.profile();
        profile.capabilities.close = true;
        profile
    }

    async fn new_session(
        &self,
        _cx: &Cx,
        request: NewSessionRequest,
    ) -> acp::Result<NewSessionResponse> {
        let opened = self.provider.open(request).await?;
        self.sessions.insert(
            opened.id.clone(),
            Slot {
                gate: TurnGate::new(self.provider.turn_policy()),
                session: opened.session,
            },
        );
        Ok(NewSessionResponse::new(opened.id).config_options(opened.options))
    }

    async fn prompt(&self, cx: &Cx, request: PromptRequest) -> acp::Result<PromptResponse> {
        let slot = self.sessions.require(&request.session_id)?;
        let turn = TurnCtx {
            turn: slot.gate.begin().await?,
            sink: Sink::new(cx, request.session_id),
        };
        let stop = self
            .provider
            .run_turn(&slot.session, request.prompt, &turn)
            .await?;
        Ok(PromptResponse::new(if turn.is_cancelled() {
            StopReason::Cancelled
        } else {
            stop
        }))
    }

    async fn cancel(&self, _cx: &Cx, session: &SessionId) {
        if let Some(slot) = self.sessions.get(session)
            && slot.gate.cancel()
        {
            self.provider.interrupt(&slot.session).await;
        }
    }

    async fn set_session_config_option(
        &self,
        _cx: &Cx,
        request: SetSessionConfigOptionRequest,
    ) -> acp::Result<SetSessionConfigOptionResponse> {
        let slot = self.sessions.require(&request.session_id)?;
        let value = request
            .value
            .as_value_id()
            .ok_or_else(|| AgentError::invalid("option values are selections"))?
            .to_string();
        let options = self
            .provider
            .configure(&slot.session, &request.config_id.to_string(), &value)
            .await?;
        Ok(SetSessionConfigOptionResponse::new(options))
    }

    async fn close_session(
        &self,
        _cx: &Cx,
        request: CloseSessionRequest,
    ) -> acp::Result<CloseSessionResponse> {
        if let Some(slot) = self.sessions.remove(&request.session_id) {
            slot.gate.cancel();
            self.provider.close(&slot.session).await;
        }
        Ok(CloseSessionResponse::new())
    }

    async fn shutdown(&self) {
        for slot in self.sessions.drain() {
            slot.gate.cancel();
            self.provider.close(&slot.session).await;
        }
    }
}
