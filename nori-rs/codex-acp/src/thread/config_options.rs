//! A session's selectable options (approval preset, model, reasoning effort) and the slash
//! commands it offers.

use crate::thread::actor::ThreadActor;
use crate::thread::backends::Auth;
use crate::thread::modes::APPROVAL_PRESETS;
use crate::thread::modes::active_profile_id_for_session_mode;
use crate::thread::modes::current_session_mode_id;
use crate::thread::modes::mode_trusts_project;
use codex_core::config::set_project_trust_level;
use codex_protocol::config_types::TrustLevel;
use codex_protocol::models::ActivePermissionProfile;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadSettingsOverrides;
use heck::ToTitleCase;
use nori_acp_kit::acp::Error;
use nori_acp_kit::schema::AvailableCommand;
use nori_acp_kit::schema::AvailableCommandInput;
use nori_acp_kit::schema::ConfigOptionUpdate;
use nori_acp_kit::schema::SessionConfigId;
use nori_acp_kit::schema::SessionConfigOption;
use nori_acp_kit::schema::SessionConfigOptionCategory;
use nori_acp_kit::schema::SessionConfigOptionValue;
use nori_acp_kit::schema::SessionConfigSelectOption;
use nori_acp_kit::schema::SessionConfigValueId;
use nori_acp_kit::schema::SessionMode;
use nori_acp_kit::schema::SessionModeId;
use nori_acp_kit::schema::SessionModeState;
use nori_acp_kit::schema::SessionUpdate;
use nori_acp_kit::schema::UnstructuredCommandInput;

impl<A: Auth> ThreadActor<A> {
    pub(super) fn builtin_commands() -> Vec<AvailableCommand> {
        vec![
            AvailableCommand::new("review", "Review my current changes and find issues").input(
                AvailableCommandInput::Unstructured(UnstructuredCommandInput::new(
                    "optional custom review instructions",
                )),
            ),
            AvailableCommand::new(
                "review-branch",
                "Review the code changes against a specific branch",
            )
            .input(AvailableCommandInput::Unstructured(
                UnstructuredCommandInput::new("branch name"),
            )),
            AvailableCommand::new(
                "review-commit",
                "Review the code changes introduced by a commit",
            )
            .input(AvailableCommandInput::Unstructured(
                UnstructuredCommandInput::new("commit sha"),
            )),
            AvailableCommand::new(
                "init",
                "create an AGENTS.md file with instructions for Codex",
            ),
            AvailableCommand::new(
                "compact",
                "summarize conversation to prevent hitting the context limit",
            ),
            AvailableCommand::new("logout", "logout of Codex"),
        ]
    }

    pub(super) fn modes(&self) -> Option<SessionModeState> {
        let current_mode_id = current_session_mode_id(&self.config)?;

        Some(SessionModeState::new(
            current_mode_id,
            APPROVAL_PRESETS
                .iter()
                .map(|preset| {
                    SessionMode::new(preset.id, preset.label).description(preset.description)
                })
                .collect(),
        ))
    }

    pub(super) async fn config_options(&self) -> Result<Vec<SessionConfigOption>, Error> {
        let mut options = Vec::new();

        if let Some(modes) = self.modes() {
            let select_options = modes
                .available_modes
                .into_iter()
                .map(|m| SessionConfigSelectOption::new(m.id.0, m.name).description(m.description))
                .collect::<Vec<_>>();

            options.push(
                SessionConfigOption::select(
                    "mode",
                    "Approval Preset",
                    modes.current_mode_id.0,
                    select_options,
                )
                .category(SessionConfigOptionCategory::Mode)
                .description("Choose an approval and sandboxing preset for your session"),
            );
        }

        let presets = self.models_manager.list_models().await;

        let current_model = self.get_current_model().await;
        let current_preset = presets.iter().find(|p| p.model == current_model).cloned();

        let mut model_select_options = Vec::new();

        if current_preset.is_none() {
            // If no preset found, return the current model string as-is
            model_select_options.push(SessionConfigSelectOption::new(
                current_model.clone(),
                current_model.clone(),
            ));
        };

        model_select_options.extend(
            presets
                .into_iter()
                .filter(|model| model.show_in_picker || model.model == current_model)
                .map(|preset| {
                    SessionConfigSelectOption::new(preset.id, preset.display_name)
                        .description(preset.description)
                }),
        );

        options.push(
            SessionConfigOption::select("model", "Model", current_model, model_select_options)
                .category(SessionConfigOptionCategory::Model)
                .description("Choose which model Codex should use"),
        );

        // Reasoning effort selector (only if the current preset exists and has >1 supported effort)
        if let Some(preset) = current_preset
            && preset.supported_reasoning_efforts.len() > 1
        {
            let supported = &preset.supported_reasoning_efforts;

            let current_effort = self
                .config
                .model_reasoning_effort
                .clone()
                .and_then(|effort| {
                    supported
                        .iter()
                        .find_map(|e| (e.effort == effort).then(|| effort.clone()))
                })
                .unwrap_or(preset.default_reasoning_effort.clone());

            let effort_select_options = supported
                .iter()
                .map(|e| {
                    SessionConfigSelectOption::new(
                        e.effort.to_string(),
                        e.effort.to_string().to_title_case(),
                    )
                    .description(e.description.clone())
                })
                .collect::<Vec<_>>();

            options.push(
                SessionConfigOption::select(
                    "reasoning_effort",
                    "Reasoning Effort",
                    current_effort.to_string(),
                    effort_select_options,
                )
                .category(SessionConfigOptionCategory::ThoughtLevel)
                .description("Choose how much reasoning effort the model should use"),
            );
        }

        Ok(options)
    }

    pub(super) async fn maybe_emit_config_options_update(&mut self) {
        let config_options = self.config_options().await.unwrap_or_default();

        if self
            .last_sent_config_options
            .as_ref()
            .is_some_and(|prev| prev == &config_options)
        {
            return;
        }

        self.last_sent_config_options = Some(config_options.clone());

        self.client
            .send_notification(SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(
                config_options,
            )));
    }

    pub(super) async fn handle_set_config_option(
        &mut self,
        config_id: SessionConfigId,
        value: SessionConfigOptionValue,
    ) -> Result<(), Error> {
        let SessionConfigOptionValue::ValueId { value } = value else {
            return Err(Error::invalid_params().data("Unsupported config option value"));
        };
        match config_id.0.as_ref() {
            "mode" => self.handle_set_mode(SessionModeId::new(value.0)).await,
            "model" => self.handle_set_config_model(value).await,
            "reasoning_effort" => self.handle_set_config_reasoning_effort(value).await,
            _ => Err(Error::invalid_params().data("Unsupported config option")),
        }
    }

    pub(super) async fn handle_set_config_model(
        &mut self,
        value: SessionConfigValueId,
    ) -> Result<(), Error> {
        let model_id = value.0;

        let presets = self.models_manager.list_models().await;
        let preset = presets.iter().find(|p| p.id.as_str() == &*model_id);

        let model_to_use = preset
            .map(|p| p.model.clone())
            .unwrap_or_else(|| model_id.to_string());

        if model_to_use.is_empty() {
            return Err(Error::invalid_params().data("No model selected"));
        }

        let effort_to_use = if let Some(preset) = preset {
            if let Some(effort) = self.config.model_reasoning_effort.clone()
                && preset
                    .supported_reasoning_efforts
                    .iter()
                    .any(|e| e.effort == effort)
            {
                Some(effort)
            } else {
                Some(preset.default_reasoning_effort.clone())
            }
        } else {
            // If the user selected a raw model string (not a known preset), don't invent a default.
            // Keep whatever was previously configured (or leave unset) so Codex can decide.
            self.config.model_reasoning_effort.clone()
        };

        self.thread
            .submit(Op::ThreadSettings {
                thread_settings: ThreadSettingsOverrides {
                    model: Some(model_to_use.clone()),
                    effort: Some(effort_to_use.clone()),
                    ..Default::default()
                },
                reply: None,
            })
            .await
            .map_err(|e| Error::from(anyhow::anyhow!(e)))?;

        self.config.model = Some(model_to_use);
        self.config.model_reasoning_effort = effort_to_use;

        Ok(())
    }

    pub(super) async fn handle_set_config_reasoning_effort(
        &mut self,
        value: SessionConfigValueId,
    ) -> Result<(), Error> {
        let effort: ReasoningEffort =
            serde_json::from_value(value.0.as_ref().into()).map_err(|_| Error::invalid_params())?;

        let current_model = self.get_current_model().await;
        let presets = self.models_manager.list_models().await;
        let Some(preset) = presets.iter().find(|p| p.model == current_model) else {
            return Err(Error::invalid_params()
                .data("Reasoning effort can only be set for known model presets"));
        };

        if !preset
            .supported_reasoning_efforts
            .iter()
            .any(|e| e.effort == effort)
        {
            return Err(
                Error::invalid_params().data("Unsupported reasoning effort for selected model")
            );
        }

        self.thread
            .submit(Op::ThreadSettings {
                thread_settings: ThreadSettingsOverrides {
                    effort: Some(Some(effort.clone())),
                    ..Default::default()
                },
                reply: None,
            })
            .await
            .map_err(|e| Error::from(anyhow::anyhow!(e)))?;

        self.config.model_reasoning_effort = Some(effort);

        Ok(())
    }

    pub(super) async fn handle_set_mode(&mut self, mode: SessionModeId) -> Result<(), Error> {
        let preset = APPROVAL_PRESETS
            .iter()
            .find(|preset| mode.0.as_ref() == preset.id)
            .ok_or_else(Error::invalid_params)?;

        self.thread
            .submit(Op::ThreadSettings {
                thread_settings: ThreadSettingsOverrides {
                    approval_policy: Some(preset.approval),
                    permission_profile: Some(preset.permission_profile.clone()),
                    active_permission_profile: active_profile_id_for_session_mode(preset.id)
                        .map(ActivePermissionProfile::new),
                    ..Default::default()
                },
                reply: None,
            })
            .await
            .map_err(|e| Error::from(anyhow::anyhow!(e)))?;

        self.config
            .permissions
            .approval_policy
            .set(preset.approval)
            .map_err(|e| Error::from(anyhow::anyhow!(e)))?;
        self.config
            .permissions
            .set_permission_profile(preset.permission_profile.clone())
            .map_err(|e| Error::from(anyhow::anyhow!(e)))?;

        if mode_trusts_project(preset.id) {
            set_project_trust_level(
                &self.config.codex_home,
                &self.config.cwd,
                TrustLevel::Trusted,
            )?;
        }

        Ok(())
    }

    pub(super) async fn get_current_model(&self) -> String {
        self.models_manager.get_model(&self.config.model).await
    }
}
