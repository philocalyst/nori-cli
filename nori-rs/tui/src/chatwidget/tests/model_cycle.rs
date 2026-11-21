use super::*;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use nori_protocol::Dial;
use nori_protocol::acp::v1 as acp;
use pretty_assertions::assert_eq;

fn models(current: &str) -> Vec<acp::SessionConfigOption> {
    vec![
        acp::SessionConfigOption::select(
            "model",
            "Model",
            current.to_owned(),
            vec![
                acp::SessionConfigSelectOption::new("sonnet", "Sonnet"),
                acp::SessionConfigSelectOption::new("opus", "Opus"),
                acp::SessionConfigSelectOption::new("haiku", "Haiku"),
            ],
        )
        .category(acp::SessionConfigOptionCategory::Model),
    ]
}

fn current_model(chat: &ChatWidget) -> String {
    let option = chat
        .agent_config()
        .option_for_category(&acp::SessionConfigOptionCategory::Model)
        .expect("a model option");
    match &option.raw.kind {
        acp::SessionConfigKind::Select(select) => select.current_value.to_string(),
        _ => panic!("the model option is a select"),
    }
}

#[test]
fn presses_step_through_the_models_and_wrap() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual();
    chat.sync_acp_session_config_snapshot(&models("sonnet"));

    chat.cycle_model(ModelStep::Next);
    assert_eq!(current_model(&chat), "opus");
    chat.cycle_model(ModelStep::Next);
    chat.cycle_model(ModelStep::Next);
    assert_eq!(current_model(&chat), "sonnet");
    chat.cycle_model(ModelStep::Previous);
    assert_eq!(current_model(&chat), "haiku");
}

fn screen(chat: &ChatWidget) -> String {
    use crate::test_backend::VT100Backend;
    use ratatui::Terminal;
    let height = chat.desired_height(80);
    let mut terminal = Terminal::new(VT100Backend::new(80, height)).expect("terminal");
    terminal
        .draw(|f| chat.render(f.area(), f.buffer_mut()))
        .expect("draw");
    terminal.backend().vt100().screen().contents()
}

#[test]
fn the_footer_shows_the_model_a_press_switched_to() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual();
    chat.sync_acp_session_config_snapshot(&models("sonnet"));
    assert!(screen(&chat).contains("Sonnet"), "{}", screen(&chat));

    chat.cycle_model(ModelStep::Next);
    let contents = screen(&chat);
    assert!(contents.contains("Opus"), "{contents}");
    assert!(!contents.contains("Sonnet"), "{contents}");

    // A refusal puts the earlier model back on screen too.
    chat.on_model_cycle_result(Err("not on this plan".to_owned()));
    assert!(screen(&chat).contains("Sonnet"), "{}", screen(&chat));
}

#[test]
fn a_refused_switch_brings_the_earlier_model_back() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual();
    chat.sync_acp_session_config_snapshot(&models("sonnet"));
    chat.cycle_model(ModelStep::Next);
    assert_eq!(current_model(&chat), "opus");
    let _ = drain_insert_history(&mut rx);

    chat.on_model_cycle_result(Err("not on this plan".to_owned()));

    assert_eq!(current_model(&chat), "sonnet");
    let history: String = drain_insert_history(&mut rx)
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect();
    assert_eq!(history, "");
}

#[test]
fn an_agent_without_a_choice_of_model_says_so() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual();
    chat.sync_acp_session_config_snapshot(&[acp::SessionConfigOption::select(
        "mode",
        "Mode",
        "ask".to_owned(),
        vec![
            acp::SessionConfigSelectOption::new("ask", "Ask"),
            acp::SessionConfigSelectOption::new("code", "Code"),
        ],
    )
    .category(acp::SessionConfigOptionCategory::Mode)]);
    chat.cycle_model(ModelStep::Next);
    assert!(std::iter::from_fn(|| rx.try_recv().ok()).any(|event| matches!(
        event, AppEvent::TransientNotice(message) if message.contains("doesn't offer a choice of model")
    )));
}

#[test]
fn a_press_before_the_agent_lists_its_models_does_nothing_yet() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual();

    assert!(!chat.cycle_model(ModelStep::Next));

    let history: String = drain_insert_history(&mut rx)
        .iter()
        .map(|lines| lines_to_single_string(lines))
        .collect();
    assert_eq!(history, "");
}

#[test]
fn presses_that_waited_for_the_list_are_made_once_it_is_known() {
    use std::time::Duration;
    use std::time::Instant;
    let t0 = Instant::now();
    let mut waiting = WaitingModelPresses::default();
    assert!(
        waiting.press(ModelStep::Next, t0),
        "the first press starts a run"
    );
    assert!(!waiting.press(ModelStep::Next, t0 + Duration::from_secs(1)));
    assert!(!waiting.press(ModelStep::Previous, t0 + Duration::from_secs(2)));
    assert!(waiting.is_waiting());
    // Two next presses and one previous leave one step forward.
    assert_eq!(
        waiting.take(t0 + Duration::from_secs(3)),
        Some((ModelStep::Next, 1))
    );
    assert!(!waiting.is_waiting());

    // A press nobody answered for a long while is forgotten rather than surfacing later.
    assert!(waiting.press(ModelStep::Next, t0));
    assert_eq!(waiting.take(t0 + Duration::from_secs(60)), None);

    // Presses that cancel out make none.
    assert!(waiting.press(ModelStep::Next, t0));
    waiting.press(ModelStep::Previous, t0);
    assert_eq!(waiting.take(t0), None);
}

#[test]
fn ctrl_n_and_ctrl_p_in_the_composer_step_the_model() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual();
    for (code, step) in [
        (KeyCode::Char('n'), ModelStep::Next),
        (KeyCode::Char('p'), ModelStep::Previous),
    ] {
        chat.handle_key_event(KeyEvent::new(code, KeyModifiers::CONTROL));
        let mut stepped = None;
        while let Ok(event) = rx.try_recv() {
            if let AppEvent::CycleModel(sent) = event {
                stepped = Some(sent);
            }
        }
        assert_eq!(stepped, Some(step));
    }
}

fn effort_options(current: &str) -> Vec<acp::SessionConfigOption> {
    vec![
        acp::SessionConfigOption::select(
            "effort",
            "Effort",
            current.to_owned(),
            vec![
                acp::SessionConfigSelectOption::new("low", "Low"),
                acp::SessionConfigSelectOption::new("high", "High"),
                acp::SessionConfigSelectOption::new("max", "Max"),
            ],
        )
        .category(acp::SessionConfigOptionCategory::ThoughtLevel),
    ]
}

fn effort_steps(rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>) -> Vec<ModelStep> {
    let mut steps = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let AppEvent::CycleEffort(step) = event {
            steps.push(step);
        }
    }
    steps
}

#[test]
fn ctrl_shift_n_and_p_step_the_effort_and_leave_the_model_keys_alone() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual();
    let both = KeyModifiers::CONTROL | KeyModifiers::SHIFT;
    // A terminal may report the shifted key as `N`.
    for (code, modifiers) in [
        (KeyCode::Char('n'), both),
        (KeyCode::Char('N'), both),
        (KeyCode::Char('p'), both),
        (KeyCode::Char('P'), both),
        (KeyCode::Char('n'), KeyModifiers::CONTROL),
    ] {
        chat.handle_key_event(KeyEvent::new(code, modifiers));
    }
    assert_eq!(
        effort_steps(&mut rx),
        vec![
            ModelStep::Next,
            ModelStep::Next,
            ModelStep::Previous,
            ModelStep::Previous
        ]
    );
}

#[test]
fn ctrl_j_and_k_step_the_effort() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual();

    for code in [KeyCode::Char('j'), KeyCode::Char('k')] {
        chat.handle_key_event(KeyEvent::new(code, KeyModifiers::CONTROL));
    }

    assert_eq!(
        effort_steps(&mut rx),
        vec![ModelStep::Next, ModelStep::Previous]
    );
}

#[test]
fn the_models_step_in_the_agents_order() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual();
    chat.sync_acp_session_config_snapshot(&[acp::SessionConfigOption::select(
        "model",
        "Model",
        "default".to_owned(),
        vec![
            acp::SessionConfigSelectOption::new("default", "Default"),
            acp::SessionConfigSelectOption::new("opus", "Opus 5.5"),
            acp::SessionConfigSelectOption::new("claude-fable-5-2", "Fable 5.2"),
            acp::SessionConfigSelectOption::new("sonnet", "Sonnet 5.5"),
            acp::SessionConfigSelectOption::new("fable", "Fable 5.1"),
            acp::SessionConfigSelectOption::new("haiku", "Haiku 5.5"),
        ],
    )
    .category(acp::SessionConfigOptionCategory::Model)]);

    let mut seen = Vec::new();
    for _ in 0..6 {
        chat.cycle_model(ModelStep::Next);
        seen.push(current_model(&chat));
    }

    assert_eq!(
        seen,
        [
            "opus",
            "claude-fable-5-2",
            "sonnet",
            "fable",
            "haiku",
            "default"
        ]
    );
}

#[test]
fn the_effort_steps_through_its_levels_and_shows_in_the_footer() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual();
    chat.sync_acp_session_config_snapshot(&models("sonnet"));
    let mut options = models("sonnet");
    options.extend(effort_options("low"));
    chat.sync_acp_session_config_snapshot(&options);
    assert!(screen(&chat).contains("Sonnet · Low"), "{}", screen(&chat));

    assert!(chat.cycle_effort(ModelStep::Next));
    assert!(screen(&chat).contains("Sonnet · High"), "{}", screen(&chat));
    chat.cycle_effort(ModelStep::Next);
    chat.cycle_effort(ModelStep::Next);
    assert!(
        screen(&chat).contains("Sonnet · Low"),
        "wraps: {}",
        screen(&chat)
    );
}

/// Presses that come while a switch is out only move the target: the agent is asked for the
/// first press's model and then for the last one, never for those in between.
#[tokio::test]
async fn presses_during_a_switch_send_only_the_last() {
    let session = super::part10::start_mock_session().await;
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual();
    chat.harness_handle = Some(session.handle.clone());
    let advertised = session
        .handle
        .get_session_config()
        .await
        .expect("the mock advertises its configuration");
    chat.sync_acp_session_config_snapshot(&advertised);
    let first = current_model(&chat);

    chat.cycle_model(ModelStep::Next);
    let after_one = current_model(&chat);
    chat.cycle_model(ModelStep::Next);
    chat.cycle_model(ModelStep::Next);
    let target = current_model(&chat);
    assert_ne!(first, after_one);

    let mut requested = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while requested.len() < 2 {
            match rx.recv().await {
                Some(AppEvent::AcpSessionConfigSetResult {
                    success,
                    value,
                    config_options,
                    error,
                    ..
                }) => {
                    assert!(success, "{error:?}");
                    requested.push(value);
                    chat.on_model_cycle_result(Ok(config_options));
                }
                Some(_) => {}
                None => panic!("event channel closed"),
            }
        }
    })
    .await
    .expect("both switches complete");

    assert_eq!(requested, vec![after_one, target.clone()]);
    assert_eq!(current_model(&chat), target);
    let _ = session.handle.shutdown().await;
}

#[test]
fn model_picker_shows_advertised_models_across_providers() {
    let home = tempfile::tempdir().expect("temporary Nori home");
    std::fs::write(
        home.path().join("agent-models.json"),
        r#"{"mock-model-alt":{"models":[{"value":"fast","name":"Fast"}],"settings":[],"refreshed_at":1}}"#,
    ).expect("cached advertised model");
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual();
    chat.bottom_pane
        .set_composer_placeholder_for_test("! for shell commands");
    chat.config.nori_home = home.path().to_path_buf();
    chat.config.active_agent = "mock-model".to_string();
    chat.sync_acp_session_config_snapshot(&models("sonnet"));
    chat.open_model_popup();
    let labels = shown_choice_labels(&mut rx);
    assert!(
        labels.iter().any(|label| label.contains("Fast")),
        "{labels:?}"
    );
    assert!(
        labels.iter().all(|label| !label.contains("Mock ACP Alt")),
        "{labels:?}"
    );
    assert!(
        labels
            .iter()
            .all(|label| !label.contains("Use custom model")),
        "{labels:?}"
    );
}

#[test]
fn opencode_model_picker_shows_clean_model_names() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual();
    chat.bottom_pane
        .set_composer_placeholder_for_test("! for shell commands");
    chat.config.active_agent = "opencode".to_string();
    chat.sync_acp_session_config_snapshot(&[acp::SessionConfigOption::select(
        "model",
        "Model",
        "opencode/big-pickle",
        vec![
            acp::SessionConfigSelectOption::new("opencode/big-pickle", "opencode/Big Pickle"),
            acp::SessionConfigSelectOption::new("local/coder", "local/Coder"),
        ],
    )
    .category(acp::SessionConfigOptionCategory::Model)]);
    chat.open_model_popup();
    let labels = shown_choice_labels(&mut rx);
    assert!(
        labels.iter().any(|label| label.contains("Big Pickle")),
        "{labels:?}"
    );
    assert!(
        labels.iter().any(|label| label.contains("Coder")),
        "{labels:?}"
    );
    assert!(
        labels
            .iter()
            .all(|label| !label.contains("opencode/") && !label.contains("local/")),
        "{labels:?}"
    );
}

#[test]
fn provider_picker_shows_opencode_identity() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual();
    chat.bottom_pane
        .set_composer_placeholder_for_test("! for shell commands");
    let mut params = crate::nori::agent_picker::provider_picker_params();
    params.items.retain(|item| item.name == "OpenCode");
    assert_eq!(params.items.len(), 1);
    params.items[0].description =
        Some("Available · sign-in status unknown · select to connect".to_string());
    chat.bottom_pane.show_selection_view(params);
    let labels = shown_choice_labels(&mut rx);
    assert!(
        labels.iter().any(|label| label.contains("OpenCode")),
        "{labels:?}"
    );
}

fn declared(
    value: &str,
    label: &str,
    effect: &nori_protocol::DialEffect,
) -> acp::SessionConfigSelectOption {
    acp::SessionConfigSelectOption::new(value.to_owned(), label.to_owned()).meta(effect.to_meta())
}

#[test]
fn dial_effect_reads_what_the_choice_declares() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual();
    let costly = nori_protocol::DialEffect {
        when: nori_protocol::AppliesAt::NextStep,
        costs: vec![nori_protocol::Consequence::PromptCacheRestarts],
    };
    chat.sync_acp_session_config_snapshot(&[acp::SessionConfigOption::select(
        "model",
        "Model",
        "sonnet".to_owned(),
        vec![
            acp::SessionConfigSelectOption::new("sonnet", "Sonnet"),
            declared("opus", "Opus", &costly),
        ],
    )
    .category(acp::SessionConfigOptionCategory::Model)]);

    assert_eq!(chat.dial_effect(Dial::Model, "opus", true), costly);
    assert_eq!(chat.dial_effect(Dial::Model, "opus", false), costly);
}

#[test]
fn dial_effect_of_an_undeclared_choice_is_next_turn_with_no_costs() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual();
    chat.sync_acp_session_config_snapshot(&models("sonnet"));

    for dial in [Dial::Model, Dial::Effort, Dial::Mode] {
        assert_eq!(
            chat.dial_effect(dial, "opus", false),
            nori_protocol::DialEffect::next_turn()
        );
    }
    assert_eq!(
        chat.dial_effect(Dial::Model, "haiku", true),
        nori_protocol::DialEffect::next_turn()
    );
}

#[test]
fn dial_effect_of_another_agents_model_restarts_the_session() {
    let (mut chat, _rx, _op_rx) = make_chatwidget_manual();
    chat.sync_acp_session_config_snapshot(&models("sonnet"));
    let restarts = |when| nori_protocol::DialEffect {
        when,
        costs: vec![nori_protocol::Consequence::SessionRestarts],
    };

    assert_eq!(
        chat.dial_effect(Dial::Model, "gpt-5/codex", true),
        restarts(nori_protocol::AppliesAt::AfterThisTurn)
    );
    assert_eq!(
        chat.dial_effect(Dial::Model, "gpt-5/codex", false),
        restarts(nori_protocol::AppliesAt::Now)
    );
}
