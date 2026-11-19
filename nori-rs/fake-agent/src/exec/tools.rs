//! Tool calls: the lifecycle, the faults in its order, and permission requests.

use nori_acp_kit::PermissionDecision;
use nori_acp_kit::schema::Diff;
use nori_acp_kit::schema::Meta;
use nori_acp_kit::schema::PermissionOption;
use nori_acp_kit::schema::PermissionOptionKind;
use nori_acp_kit::schema::SessionUpdate;
use nori_acp_kit::schema::Terminal;
use nori_acp_kit::schema::ToolCall;
use nori_acp_kit::schema::ToolCallContent;
use nori_acp_kit::schema::ToolCallLocation;
use nori_acp_kit::schema::ToolCallStatus;
use nori_acp_kit::schema::ToolCallUpdate;
use nori_acp_kit::schema::ToolCallUpdateFields;
use nori_acp_kit::schema::ToolKind;
use serde_json::Value;
use serde_json::json;

use super::Break;
use super::Ctx;
use super::Run;
use crate::record::Record;
use crate::world::Answer;
use crate::world::ContentDef;
use crate::world::Disorder;
use crate::world::Location;
use crate::world::OptionDef;
use crate::world::OptionKindDef;
use crate::world::PermissionStep;
use crate::world::Progress;
use crate::world::ToolKindDef;
use crate::world::ToolOutcome;
use crate::world::ToolSpec;
use crate::world::ToolStart;
use crate::world::ToolStatusDef;
use crate::world::ToolUpdateStep;

fn kind(kind: ToolKindDef) -> ToolKind {
    match kind {
        ToolKindDef::Read => ToolKind::Read,
        ToolKindDef::Edit => ToolKind::Edit,
        ToolKindDef::Delete => ToolKind::Delete,
        ToolKindDef::Move => ToolKind::Move,
        ToolKindDef::Search => ToolKind::Search,
        ToolKindDef::Execute => ToolKind::Execute,
        ToolKindDef::Think => ToolKind::Think,
        ToolKindDef::Fetch => ToolKind::Fetch,
        ToolKindDef::SwitchMode => ToolKind::SwitchMode,
        ToolKindDef::Other => ToolKind::Other,
    }
}

fn status(status: ToolStatusDef) -> ToolCallStatus {
    match status {
        ToolStatusDef::Pending => ToolCallStatus::Pending,
        ToolStatusDef::InProgress => ToolCallStatus::InProgress,
        ToolStatusDef::Completed => ToolCallStatus::Completed,
        ToolStatusDef::Failed => ToolCallStatus::Failed,
    }
}

fn locations(locations: &[Location]) -> Vec<ToolCallLocation> {
    locations
        .iter()
        .map(|location| {
            ToolCallLocation::new(location.path.clone())
                .line(location.line.and_then(|line| u32::try_from(line).ok()))
        })
        .collect()
}

fn meta_of(value: Option<&Value>) -> Option<Meta> {
    match value {
        Some(Value::Object(map)) => Some(Meta::from_iter(map.clone())),
        Some(_) | None => None,
    }
}

/// The `raw_input` Nori's own agents report, derived from what the call shows when a script
/// did not write one: an edit's file and strings, a read's path.
fn derive_input(spec: &ToolSpec) -> Option<Value> {
    if spec.input.is_some() {
        return spec.input.clone();
    }
    let diffs: Vec<&ContentDef> = spec
        .content
        .iter()
        .chain(spec.result.iter().flatten())
        .filter(|content| matches!(content, ContentDef::Diff { .. }))
        .collect();
    match (spec.kind, diffs.as_slice()) {
        (
            ToolKindDef::Edit,
            [
                ContentDef::Diff {
                    path,
                    old_text,
                    new_text,
                },
            ],
        ) => Some(match old_text {
            Some(old) => json!({"file_path": path, "old_string": old, "new_string": new_text}),
            None => json!({"file_path": path, "content": new_text}),
        }),
        (ToolKindDef::Read, _) => spec
            .locations
            .first()
            .map(|location| json!({"file_path": location.path})),
        (
            ToolKindDef::Delete
            | ToolKindDef::Move
            | ToolKindDef::Search
            | ToolKindDef::Execute
            | ToolKindDef::Think
            | ToolKindDef::Fetch
            | ToolKindDef::SwitchMode
            | ToolKindDef::Other
            | ToolKindDef::Edit,
            _,
        ) => None,
    }
}

impl Ctx {
    /// A fresh call id when the script gave none.
    pub(super) fn tool_id(&self, given: Option<&str>) -> String {
        match given {
            Some(id) => self.render(id),
            None => {
                let mut state = self.live.record.state();
                state.tool_calls += 1;
                format!("{}-tool-{}", self.agent(), state.tool_calls)
            }
        }
    }

    pub(super) fn content(&self, content: &[ContentDef]) -> Vec<ToolCallContent> {
        content
            .iter()
            .map(|content| match content {
                ContentDef::Text { text } => ToolCallContent::from(self.text(text)),
                ContentDef::Diff {
                    path,
                    old_text,
                    new_text,
                } => ToolCallContent::from(
                    Diff::new(path.clone(), new_text.clone()).old_text(old_text.clone()),
                ),
                ContentDef::Terminal { terminal_id } => {
                    ToolCallContent::Terminal(Terminal::new(terminal_id.clone()))
                }
            })
            .collect()
    }

    fn announcement(&self, id: &str, spec: &ToolSpec, input: Option<Value>) -> ToolCall {
        let mut meta = meta_of(spec.meta.as_ref()).unwrap_or_default();
        if let Some(name) = &spec.host_tool {
            meta.extend(nori_host_tools::tool_call_meta(name));
        }
        ToolCall::new(id.to_owned(), self.render(&spec.title))
            .kind(kind(spec.kind))
            .status(status(spec.status))
            .content(self.content(&spec.content))
            .locations(locations(&spec.locations))
            .raw_input(input)
            .meta((!meta.is_empty()).then_some(meta))
    }

    /// The terminal update: status, final content, and the output the tool produced.
    fn ending(
        &self,
        id: &str,
        spec: &ToolSpec,
        outcome: ToolOutcome,
        stdout: &str,
    ) -> ToolCallUpdate {
        let mut content = self.content(spec.result.as_deref().unwrap_or(&spec.content));
        if !stdout.is_empty() {
            content.push(ToolCallContent::from(stdout.to_owned()));
        }
        let stderr = spec
            .stderr
            .as_ref()
            .map(|text| self.text(text))
            .unwrap_or_default();
        if !stderr.is_empty() {
            content.push(ToolCallContent::from(stderr.clone()));
        }
        let mut output = match &spec.output {
            Some(Value::Object(map)) => map.clone(),
            Some(other) => serde_json::Map::from_iter([("output".to_owned(), other.clone())]),
            None => serde_json::Map::new(),
        };
        if !stdout.is_empty() {
            output.insert("stdout".to_owned(), json!(stdout));
        }
        if !stderr.is_empty() {
            output.insert("stderr".to_owned(), json!(stderr));
        }
        if let Some(code) = spec.exit_code {
            output.insert("exit_code".to_owned(), json!(code));
        }
        let fields = ToolCallUpdateFields::new()
            .status(match outcome {
                ToolOutcome::Completed => ToolCallStatus::Completed,
                ToolOutcome::Failed => ToolCallStatus::Failed,
            })
            .content((!content.is_empty()).then_some(content))
            .raw_output((!output.is_empty()).then_some(Value::Object(output)));
        ToolCallUpdate::new(id.to_owned(), fields).meta(meta_of(spec.meta.as_ref()))
    }

    fn progress_update(&self, id: &str, progress: &Progress, stdout: &str) -> ToolCallUpdate {
        let mut fields = ToolCallUpdateFields::new()
            .status(status(progress.status.unwrap_or(ToolStatusDef::InProgress)))
            .title(progress.title.as_ref().map(|title| self.render(title)));
        let content = match &progress.content {
            Some(content) => Some(self.content(content)),
            None if progress.stdout.is_some() => {
                Some(vec![ToolCallContent::from(stdout.to_owned())])
            }
            None => None,
        };
        fields = fields.content(content);
        let output = match (&progress.output, progress.stdout.is_some()) {
            (Some(Value::Object(map)), true) => {
                let mut map = map.clone();
                map.insert("stdout".to_owned(), json!(stdout));
                Some(Value::Object(map))
            }
            (Some(other), false) => Some(other.clone()),
            (Some(_) | None, true) => Some(json!({"stdout": stdout})),
            (None, false) => None,
        };
        ToolCallUpdate::new(id.to_owned(), fields.raw_output(output))
    }

    /// A complete call. `announced` when a permission request already announced it.
    pub(super) async fn tool(&self, spec: &ToolSpec, announced: bool) -> Run {
        let id = self.tool_id(spec.id.as_deref());
        self.tool_with_id(&id, spec, announced).await
    }

    pub(super) async fn tool_with_id(&self, id: &str, spec: &ToolSpec, announced: bool) -> Run {
        let input = derive_input(spec);
        let mut stdout = spec
            .stdout
            .as_ref()
            .map(|text| self.text(text))
            .unwrap_or_default();
        // Progress output comes before the final output.
        let mut streamed = String::new();
        let outcome = spec.outcome;
        if !announced {
            if spec.disorder == Disorder::EndBeforeStart {
                self.emit(SessionUpdate::ToolCallUpdate(
                    self.ending(id, spec, outcome, &stdout),
                ))?;
            }
            let call = self.announcement(id, spec, input);
            self.emit(SessionUpdate::ToolCall(call.clone()))?;
            if spec.disorder == Disorder::DuplicateStart {
                self.emit(SessionUpdate::ToolCall(call))?;
            }
        }
        if spec.disorder == Disorder::EndBeforeStart {
            return Ok(());
        }
        if spec.status == ToolStatusDef::Pending {
            self.emit(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                id.to_owned(),
                ToolCallUpdateFields::new().status(ToolCallStatus::InProgress),
            )))?;
        }
        if spec.disorder == Disorder::NeverEnds {
            return Ok(());
        }
        let mut spent = std::time::Duration::ZERO;
        for progress in &spec.progress {
            self.sleep(progress.after.0).await?;
            spent += progress.after.0;
            if let Some(text) = &progress.stdout {
                streamed.push_str(&self.text(text));
            }
            self.emit(SessionUpdate::ToolCallUpdate(
                self.progress_update(id, progress, &streamed),
            ))?;
        }
        self.sleep(spec.duration.0.saturating_sub(spent)).await?;
        stdout.insert_str(0, &streamed);
        self.emit(SessionUpdate::ToolCallUpdate(
            self.ending(id, spec, outcome, &stdout),
        ))?;
        match spec.disorder {
            Disorder::UpdateAfterEnd => {
                self.emit(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                    id.to_owned(),
                    ToolCallUpdateFields::new().status(ToolCallStatus::InProgress),
                )))
            }
            Disorder::ContradictoryEnd => {
                let opposite = match outcome {
                    ToolOutcome::Completed => ToolOutcome::Failed,
                    ToolOutcome::Failed => ToolOutcome::Completed,
                };
                self.emit(SessionUpdate::ToolCallUpdate(
                    self.ending(id, spec, opposite, &stdout),
                ))
            }
            Disorder::None
            | Disorder::EndBeforeStart
            | Disorder::DuplicateStart
            | Disorder::NeverEnds => Ok(()),
        }
    }

    pub(super) fn tool_start(&self, start: &ToolStart) -> Run {
        let spec = ToolSpec {
            id: Some(start.id.clone()),
            title: start.title.clone(),
            kind: start.kind,
            locations: start.locations.clone(),
            input: start.input.clone(),
            status: start.status,
            content: start.content.clone(),
            meta: start.meta.clone(),
            ..ToolSpec::default()
        };
        let id = self.tool_id(spec.id.as_deref());
        let call = self.announcement(&id, &spec, derive_input(&spec));
        self.emit(SessionUpdate::ToolCall(call))
    }

    pub(super) fn tool_update(&self, update: &ToolUpdateStep) -> Run {
        let fields = ToolCallUpdateFields::new()
            .status(update.status.map(status))
            .title(update.title.as_ref().map(|title| self.render(title)))
            .content(update.content.as_ref().map(|content| self.content(content)))
            .locations(update.locations.as_deref().map(locations))
            .raw_output(update.output.clone());
        self.emit(SessionUpdate::ToolCallUpdate(
            ToolCallUpdate::new(self.render(&update.id), fields)
                .meta(meta_of(update.meta.as_ref())),
        ))
    }

    fn permission_options(options: &[OptionDef]) -> Vec<OptionDef> {
        if !options.is_empty() {
            return options.to_vec();
        }
        [
            ("allow", "Allow", OptionKindDef::AllowOnce),
            ("allow_always", "Always allow", OptionKindDef::AllowAlways),
            ("reject", "Deny", OptionKindDef::RejectOnce),
        ]
        .into_iter()
        .map(|(id, name, kind)| OptionDef {
            id: id.to_owned(),
            name: name.to_owned(),
            kind,
        })
        .collect()
    }

    /// Ask permission for a tool, then branch on the answer.
    pub(super) async fn permission(&self, step: &PermissionStep) -> Run {
        let spec = &step.tool;
        let id = self.tool_id(spec.id.as_deref());
        let input = derive_input(spec);
        // The call is announced first, as agents do; the request refers to it by id.
        self.emit(SessionUpdate::ToolCall(self.announcement(
            &id,
            spec,
            input.clone(),
        )))?;
        let options = Self::permission_options(&step.options);
        let request = ToolCallUpdate::new(
            id.clone(),
            ToolCallUpdateFields::new()
                .title(self.render(&spec.title))
                .kind(kind(spec.kind))
                .content(Some(self.content(&spec.content)).filter(|content| !content.is_empty()))
                .locations(
                    Some(locations(&spec.locations)).filter(|locations| !locations.is_empty()),
                )
                .raw_input(input),
        );
        let offered: Vec<PermissionOption> = options
            .iter()
            .map(|option| {
                PermissionOption::new(
                    option.id.clone(),
                    option.name.clone(),
                    match option.kind {
                        OptionKindDef::AllowOnce => PermissionOptionKind::AllowOnce,
                        OptionKindDef::AllowAlways => PermissionOptionKind::AllowAlways,
                        OptionKindDef::RejectOnce => PermissionOptionKind::RejectOnce,
                        OptionKindDef::RejectAlways => PermissionOptionKind::RejectAlways,
                    },
                )
            })
            .collect();
        self.record(Record::PermissionAsked {
            agent: self.agent().to_owned(),
            session: self.session_id().to_owned(),
            tool: spec.title.clone(),
            outside_turn: !self
                .live
                .turn_open
                .load(std::sync::atomic::Ordering::SeqCst),
        });
        let ask = self.lane.sink.ask_permission(request, offered);
        let decision = match step.timeout {
            None => Some(self.outside(ask).await?),
            Some(timeout) => {
                let asker = self.participant.join();
                tokio::select! {
                    decision = asker.external(ask) => Some(decision),
                    () = self.participant.sleep(timeout.0) => None,
                    () = self.env.shutdown.cancelled() => return Err(Break::Gone),
                }
            }
        };
        let answer = match decision {
            None => Answer::Timeout,
            Some(Ok(PermissionDecision::Selected(option))) => {
                match options
                    .iter()
                    .find(|candidate| candidate.id == option.to_string())
                {
                    Some(chosen) if chosen.kind.allows() => Answer::Allow,
                    Some(_) => Answer::Deny,
                    None => {
                        self.diverge(format!(
                            "the client chose option {option}, which was not offered"
                        ));
                        Answer::Deny
                    }
                }
            }
            Some(Ok(PermissionDecision::Cancelled)) => Answer::Cancelled,
            Some(Err(error)) => {
                self.diverge(format!("the permission request failed: {error}"));
                Answer::Cancelled
            }
        };
        self.record(Record::PermissionAnswered {
            agent: self.agent().to_owned(),
            session: self.session_id().to_owned(),
            tool: spec.title.clone(),
            answer,
        });
        if let Some(expected) = step.expect
            && expected != answer
        {
            self.diverge(format!(
                "the script expected {expected:?} for {:?} and got {answer:?}",
                spec.title
            ));
        }
        match answer {
            Answer::Allow => {
                if step.run_when_allowed {
                    self.tool_with_id(&id, spec, true).await?;
                }
                self.run(&step.on_allow).await
            }
            Answer::Deny => {
                self.fail_call(&id, "Permission denied.")?;
                self.run(&step.on_deny).await
            }
            Answer::Cancelled => {
                self.fail_call(&id, "Permission request cancelled.")?;
                self.run(&step.on_cancelled).await
            }
            Answer::Timeout => {
                self.fail_call(&id, "Permission request timed out.")?;
                self.run(&step.on_timeout).await
            }
        }
    }

    fn fail_call(&self, id: &str, reason: &str) -> Run {
        self.emit(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            id.to_owned(),
            ToolCallUpdateFields::new()
                .status(ToolCallStatus::Failed)
                .content(Some(vec![ToolCallContent::from(reason.to_owned())])),
        )))
    }
}
