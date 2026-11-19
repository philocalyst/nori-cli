//! The fake agent: a [`NativeAgent`] whose sessions answer from a world's scenes.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use nori_acp_kit::AgentError;
use nori_acp_kit::Capabilities;
use nori_acp_kit::Choice;
use nori_acp_kit::Cx;
use nori_acp_kit::HostBridge;
use nori_acp_kit::NativeAgent;
use nori_acp_kit::Options;
use nori_acp_kit::Profile;
use nori_acp_kit::Select;
use nori_acp_kit::Sessions;
use nori_acp_kit::Sink;
use nori_acp_kit::TurnGate;
use nori_acp_kit::TurnPolicy;
use nori_acp_kit::acp;
use nori_acp_kit::async_trait;
use nori_acp_kit::schema::AuthMethod;
use nori_acp_kit::schema::AuthMethodAgent;
use nori_acp_kit::schema::AuthenticateRequest;
use nori_acp_kit::schema::AuthenticateResponse;
use nori_acp_kit::schema::AvailableCommand;
use nori_acp_kit::schema::AvailableCommandInput;
use nori_acp_kit::schema::CloseSessionRequest;
use nori_acp_kit::schema::CloseSessionResponse;
use nori_acp_kit::schema::ContentBlock;
use nori_acp_kit::schema::ContentChunk;
use nori_acp_kit::schema::CurrentModeUpdate;
use nori_acp_kit::schema::DeleteSessionRequest;
use nori_acp_kit::schema::DeleteSessionResponse;
use nori_acp_kit::schema::ForkSessionRequest;
use nori_acp_kit::schema::ForkSessionResponse;
use nori_acp_kit::schema::ListSessionsRequest;
use nori_acp_kit::schema::ListSessionsResponse;
use nori_acp_kit::schema::LoadSessionRequest;
use nori_acp_kit::schema::LoadSessionResponse;
use nori_acp_kit::schema::Meta;
use nori_acp_kit::schema::NewSessionRequest;
use nori_acp_kit::schema::NewSessionResponse;
use nori_acp_kit::schema::PromptRequest;
use nori_acp_kit::schema::PromptResponse;
use nori_acp_kit::schema::ResumeSessionRequest;
use nori_acp_kit::schema::ResumeSessionResponse;
use nori_acp_kit::schema::SessionConfigOptionCategory;
use nori_acp_kit::schema::SessionId;
use nori_acp_kit::schema::SessionInfo;
use nori_acp_kit::schema::SessionMode;
use nori_acp_kit::schema::SessionModeState;
use nori_acp_kit::schema::SessionUpdate;
use nori_acp_kit::schema::SetSessionConfigOptionRequest;
use nori_acp_kit::schema::SetSessionConfigOptionResponse;
use nori_acp_kit::schema::SetSessionModeRequest;
use nori_acp_kit::schema::SetSessionModeResponse;
use serde_json::json;

use crate::exec::Env;
use crate::exec::Live;
use crate::exec::errors;
use crate::record::Record;
use crate::runtime::WorldRuntime;
use crate::session::SessionRecord;
use crate::wire::Wire;
use crate::world::AgentDef;
use crate::world::TurnPolicyDef;
use crate::world::WorldError;

/// The wire name and title live for the process: `Profile` wants `&'static str`, and a world has
/// a handful of agents. Interned so serving the same agent repeatedly leaks nothing more.
fn interned(text: &str) -> &'static str {
    static NAMES: LazyLock<Mutex<HashSet<&'static str>>> = LazyLock::new(Mutex::default);
    let mut names = NAMES.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(existing) = names.get(text) {
        return existing;
    }
    let leaked: &'static str = Box::leak(text.to_owned().into_boxed_str());
    names.insert(leaked);
    leaked
}

fn options_of(def: &AgentDef) -> Options {
    let mut selects = Vec::new();
    if let Some(initial) = def.initial_model() {
        selects.push(Select::model(
            def.models
                .iter()
                .map(|model| {
                    let choice = Choice::new(
                        model.id.clone(),
                        model.name.clone().unwrap_or_else(|| model.id.clone()),
                    );
                    match &model.description {
                        Some(description) => choice.describe(description.clone()),
                        None => choice,
                    }
                })
                .collect(),
            initial.id.clone(),
        ));
    }
    if let Some(first) = def.efforts.first() {
        selects.push(Select::thought_level(
            def.efforts
                .iter()
                .map(|effort| Choice::new(effort.clone(), effort.clone()))
                .collect(),
            first.clone(),
        ));
    }
    if let Some(initial) = def
        .default_mode
        .as_ref()
        .or_else(|| def.modes.first().map(|mode| &mode.id))
    {
        selects.push(Select::new(
            nori_acp_kit::MODE,
            "Mode",
            SessionConfigOptionCategory::Mode,
            def.modes
                .iter()
                .map(|mode| {
                    Choice::new(
                        mode.id.clone(),
                        mode.name.clone().unwrap_or_else(|| mode.id.clone()),
                    )
                })
                .collect(),
            initial.clone(),
        ));
    }
    Options::new(selects)
}

fn mode_state(def: &AgentDef, options: &Options) -> Option<SessionModeState> {
    let current = options.current(nori_acp_kit::MODE)?;
    Some(SessionModeState::new(
        current.to_owned(),
        def.modes
            .iter()
            .map(|mode| {
                SessionMode::new(
                    mode.id.clone(),
                    mode.name.clone().unwrap_or_else(|| mode.id.clone()),
                )
                .description(mode.description.clone())
            })
            .collect(),
    ))
}

/// One connection of one fake agent of a world.
pub struct FakeAgent {
    env: Arc<Env>,
    sessions: Sessions<Live>,
    authenticated: AtomicBool,
}

impl FakeAgent {
    /// The agent `slug` of `runtime`'s world, speaking on `wire`.
    pub fn new(runtime: Arc<WorldRuntime>, slug: &str, wire: Wire) -> Result<Self, WorldError> {
        let def = runtime
            .world
            .agent(slug)
            .cloned()
            .ok_or_else(|| WorldError::Invalid {
                origin: runtime.world.name.clone(),
                message: format!(
                    "the world has no agent {slug:?}; it has: {}",
                    runtime.world.slugs().join(", ")
                ),
            })?;
        runtime.recorder.push(
            runtime.clock.elapsed(),
            Record::Serving {
                agent: slug.to_owned(),
            },
        );
        Ok(Self {
            env: Arc::new(Env {
                rt: runtime,
                def: Arc::new(def),
                wire,
                shutdown: tokio_util::sync::CancellationToken::new(),
            }),
            sessions: Sessions::default(),
            authenticated: AtomicBool::new(false),
        })
    }

    fn live(&self, id: &SessionId) -> Result<Arc<Live>, AgentError> {
        self.sessions.require(id)
    }

    fn attach(&self, record: Arc<SessionRecord>, meta: Option<&Meta>) -> Arc<Live> {
        let host = if self.env.def.capabilities.host_tools {
            HostBridge::from_session_meta(meta, &record.id)
        } else {
            None
        };
        self.sessions.insert(
            SessionId::new(record.id.clone()),
            Live {
                record,
                gate: TurnGate::new(match self.env.def.turn_policy {
                    TurnPolicyDef::Exclusive => TurnPolicy::Exclusive,
                    TurnPolicyDef::Steerable => TurnPolicy::Steerable,
                    TurnPolicyDef::Replace => TurnPolicy::Replace,
                }),
                host,
                turn_open: AtomicBool::new(false),
            },
        )
    }

    fn new_record(&self, cwd: std::path::PathBuf) -> Arc<SessionRecord> {
        let rt = &self.env.rt;
        let number = rt.next("sessions");
        let id = format!("{}-session-{}", self.env.def.slug, number + 1);
        rt.insert_session(SessionRecord::new(
            id,
            self.env.def.slug.clone(),
            number,
            cwd,
            rt.clock.now_unix(),
            options_of(&self.env.def),
        ))
    }
}

#[async_trait]
impl NativeAgent for FakeAgent {
    fn profile(&self) -> Profile {
        let def = &self.env.def;
        let caps = def.capabilities;
        let mut profile = Profile::new(
            interned(&format!("nori-fake-{}", def.slug)),
            interned(def.title()),
            interned(&def.version),
        )
        .capabilities(Capabilities {
            image: caps.image,
            embedded_context: caps.embedded_context,
            mcp_http: caps.mcp_http,
            mcp_sse: false,
            load: caps.load,
            resume: caps.resume,
            fork: caps.fork,
            list: caps.list,
            close: caps.close,
            delete: caps.delete_sessions,
            additional_directories: caps.additional_directories,
            logout: false,
            host_tools: caps.host_tools,
        });
        if def.startup.requires_auth {
            profile = profile.auth_methods(vec![AuthMethod::Agent(AuthMethodAgent::new(
                "fake-login",
                "Sign in to the fake provider",
            ))]);
        }
        profile
    }

    async fn authenticate(
        &self,
        _cx: &Cx,
        _request: AuthenticateRequest,
    ) -> acp::Result<AuthenticateResponse> {
        self.authenticated.store(true, Ordering::SeqCst);
        Ok(AuthenticateResponse::new())
    }

    async fn new_session(
        &self,
        _cx: &Cx,
        request: NewSessionRequest,
    ) -> acp::Result<NewSessionResponse> {
        let def = &self.env.def;
        let clock = &self.env.rt.clock;
        let delay = def.startup.new_session_delay.0;
        if !delay.is_zero() {
            let participant = clock.join();
            tokio::select! {
                () = participant.sleep(delay) => {}
                () = self.env.shutdown.cancelled() => {
                    return Err(AgentError::Unreachable("the connection ended".to_owned()).into());
                }
            }
        }
        if def.startup.requires_auth && !self.authenticated.load(Ordering::SeqCst) {
            return Err(AgentError::AuthRequired(
                "this fake agent needs `authenticate` before `session/new`".to_owned(),
            )
            .into());
        }
        if let Some(error) = &def.startup.new_session_error {
            return Err(errors::acp_error(error, clock));
        }
        let record = self.new_record(request.cwd);
        let live = self.attach(Arc::clone(&record), request.meta.as_ref());
        let options = live.record.state().options.clone();
        self.env.rt.recorder.push(
            clock.elapsed(),
            Record::SessionOpened {
                agent: def.slug.clone(),
                session: record.id.clone(),
                model: record.model(),
            },
        );
        Ok(NewSessionResponse::new(record.id.clone())
            .modes(mode_state(def, &options))
            .config_options(options.to_acp()))
    }

    async fn load_session(
        &self,
        cx: &Cx,
        request: LoadSessionRequest,
    ) -> acp::Result<LoadSessionResponse> {
        let record = self.known(&request.session_id)?;
        let live = self.attach(Arc::clone(&record), request.meta.as_ref());
        let history = live.record.state().history.clone();
        let sink = Sink::new(cx, request.session_id.clone());
        for turn in history {
            sink.update(SessionUpdate::UserMessageChunk(ContentChunk::new(
                ContentBlock::from(turn.prompt),
            )))?;
            for update in turn.updates {
                let params = json!({"sessionId": request.session_id, "update": update});
                cx.send_notification(acp::UntypedMessage::new("session/update", params)?)?;
            }
        }
        let options = live.record.state().options.clone();
        Ok(LoadSessionResponse::new()
            .modes(mode_state(&self.env.def, &options))
            .config_options(options.to_acp()))
    }

    async fn resume_session(
        &self,
        _cx: &Cx,
        request: ResumeSessionRequest,
    ) -> acp::Result<ResumeSessionResponse> {
        let record = self.known(&request.session_id)?;
        let live = self.attach(record, request.meta.as_ref());
        let options = live.record.state().options.clone();
        Ok(ResumeSessionResponse::new()
            .modes(mode_state(&self.env.def, &options))
            .config_options(options.to_acp()))
    }

    async fn fork_session(
        &self,
        _cx: &Cx,
        request: ForkSessionRequest,
    ) -> acp::Result<ForkSessionResponse> {
        let rt = &self.env.rt;
        let source = self.known(&request.session_id)?;
        let number = rt.next("sessions");
        let id = format!("{}-session-{}", self.env.def.slug, number + 1);
        let forked = rt.insert_session(source.forked(id, number, rt.clock.now_unix()));
        let live = self.attach(Arc::clone(&forked), request.meta.as_ref());
        let options = live.record.state().options.clone();
        Ok(ForkSessionResponse::new(forked.id.clone())
            .modes(mode_state(&self.env.def, &options))
            .config_options(options.to_acp()))
    }

    async fn list_sessions(
        &self,
        _cx: &Cx,
        request: ListSessionsRequest,
    ) -> acp::Result<ListSessionsResponse> {
        let sessions = self
            .env
            .rt
            .sessions_of(&self.env.def.slug)
            .into_iter()
            .filter(|session| request.cwd.as_ref().is_none_or(|cwd| *cwd == session.cwd))
            .map(|session| {
                SessionInfo::new(session.id.clone(), session.cwd.clone())
                    .title(session.state().title.clone())
                    .updated_at(Some(nori_acp_kit::rfc3339_utc(session.created_at)))
            })
            .collect();
        Ok(ListSessionsResponse::new(sessions))
    }

    async fn close_session(
        &self,
        _cx: &Cx,
        request: CloseSessionRequest,
    ) -> acp::Result<CloseSessionResponse> {
        if let Some(live) = self.sessions.remove(&request.session_id) {
            live.gate.cancel();
        }
        Ok(CloseSessionResponse::new())
    }

    async fn delete_session(
        &self,
        _cx: &Cx,
        request: DeleteSessionRequest,
    ) -> acp::Result<DeleteSessionResponse> {
        if let Some(live) = self.sessions.remove(&request.session_id) {
            live.gate.cancel();
        }
        self.env.rt.remove_session(&request.session_id.to_string());
        Ok(DeleteSessionResponse::new())
    }

    async fn set_session_mode(
        &self,
        cx: &Cx,
        request: SetSessionModeRequest,
    ) -> acp::Result<SetSessionModeResponse> {
        let live = self.live(&request.session_id)?;
        let mode = request.mode_id.to_string();
        live.record.state().options.set(nori_acp_kit::MODE, &mode)?;
        Sink::new(cx, request.session_id).update(SessionUpdate::CurrentModeUpdate(
            CurrentModeUpdate::new(mode),
        ))?;
        Ok(SetSessionModeResponse::new())
    }

    async fn set_session_config_option(
        &self,
        _cx: &Cx,
        request: SetSessionConfigOptionRequest,
    ) -> acp::Result<SetSessionConfigOptionResponse> {
        let live = self.live(&request.session_id)?;
        let value = request
            .value
            .as_value_id()
            .ok_or_else(|| AgentError::invalid("option values are selections"))?
            .to_string();
        let option = request.config_id.to_string();
        let options = live.record.state().options.set(&option, &value)?;
        self.env.rt.recorder.push(
            self.env.rt.clock.elapsed(),
            Record::ConfigChanged {
                agent: self.env.def.slug.clone(),
                session: live.record.id.clone(),
                option,
                value,
            },
        );
        Ok(SetSessionConfigOptionResponse::new(options))
    }

    async fn prompt(&self, cx: &Cx, request: PromptRequest) -> acp::Result<PromptResponse> {
        let live = self.live(&request.session_id)?;
        crate::exec::run_prompt(&self.env, &live, cx, request).await
    }

    async fn cancel(&self, _cx: &Cx, session: &SessionId) {
        self.env.rt.recorder.push(
            self.env.rt.clock.elapsed(),
            Record::CancelReceived {
                agent: self.env.def.slug.clone(),
                session: session.to_string(),
            },
        );
        if let Some(live) = self.sessions.get(session) {
            live.gate.cancel();
        }
    }

    async fn session_ready(&self, cx: &Cx, session: &SessionId) {
        if self.env.def.commands.is_empty() {
            return;
        }
        let commands = self
            .env
            .def
            .commands
            .iter()
            .map(|command| {
                AvailableCommand::new(command.name.clone(), command.description.clone()).input(
                    command.hint.clone().map(|hint| {
                        AvailableCommandInput::Unstructured(
                            nori_acp_kit::schema::UnstructuredCommandInput::new(hint),
                        )
                    }),
                )
            })
            .collect();
        let _ = Sink::new(cx, session.clone()).commands(commands);
    }

    async fn shutdown(&self) {
        self.env.shutdown.cancel();
        for live in self.sessions.drain() {
            live.gate.cancel();
        }
    }
}

impl FakeAgent {
    fn known(&self, id: &SessionId) -> Result<Arc<SessionRecord>, AgentError> {
        self.env
            .rt
            .session(&id.to_string())
            .filter(|record| record.agent == self.env.def.slug)
            .ok_or_else(|| AgentError::unknown_session(id))
    }
}
