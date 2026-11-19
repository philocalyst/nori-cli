//! Flat model choices with their underlying ACP routes kept together.
use std::path::Path;

use super::session_config_picker::ConfigPickerOrigin;
use crate::app_event::AppEvent;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use nori_config::NoriConfig as Config;
use nori_harness::recovery::Availability;
use nori_harness::recovery::ModelIdentity;
use nori_harness::recovery::RouteRanking;
use nori_protocol::RouteRef;
use nori_protocol::acp::v1 as acp;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ModelRoute {
    Session {
        agent_name: String,
        config_id: String,
        value: String,
        option_name: String,
    },
    Agent {
        agent_name: String,
        display_name: String,
        model: String,
    },
}

#[derive(Clone)]
struct ModelChoice {
    name: String,
    identity: ModelIdentity,
    routes: Vec<ModelRoute>,
    current: bool,
    /// Why no route of this model can run, when none can. Such a choice is listed, not
    /// offered.
    unavailable: Option<String>,
}

/// The name of the agent a route runs on.
fn agent_label(route: &ModelRoute) -> String {
    match route {
        ModelRoute::Session { agent_name, .. } => nori_harness::get_agent_display_name(agent_name),
        ModelRoute::Agent { display_name, .. } => display_name.clone(),
    }
}

fn route_ref(route: &ModelRoute) -> RouteRef {
    match route {
        ModelRoute::Session {
            agent_name, value, ..
        } => RouteRef::new(agent_name.clone(), Some(value.clone())),
        ModelRoute::Agent {
            agent_name, model, ..
        } => RouteRef::new(agent_name.clone(), Some(model.clone())),
    }
}

fn add(choices: &mut Vec<ModelChoice>, name: &str, value: &str, route: ModelRoute, current: bool) {
    let identity = ModelIdentity::of(name, value);
    if let Some(choice) = choices
        .iter_mut()
        .find(|choice| choice.unavailable.is_none() && choice.identity.is_same_model(&identity))
    {
        if !choice.routes.contains(&route) {
            choice.routes.push(route);
        }
        choice.current |= current;
        if choice.identity.version.is_none() {
            choice.identity.version = identity.version;
            choice.identity.variant = identity.variant;
        }
    } else {
        let display = if identity.key == "automatic" {
            "Automatic".to_owned()
        } else {
            name.to_owned()
        };
        choices.push(ModelChoice {
            name: display,
            identity,
            routes: vec![route],
            current,
            unavailable: None,
        });
    }
}

/// The model choices to offer, best first, each with its routes ranked by `ranking`.
///
/// Routes come from the agent's own advertised options and from the harness route catalog,
/// which never contains debug fixtures and carries the reason a model cannot run. Ordering is
/// the harness's one ranking: the running route, then explicit preferences, then native
/// providers before gateways.
fn collect_choices(
    home: &Path,
    active_agent: Option<&str>,
    option: Option<&acp::SessionConfigOption>,
    origin: ConfigPickerOrigin,
    ranking: &RouteRanking,
) -> Vec<ModelChoice> {
    let mut choices = Vec::new();
    let mut unavailable: Vec<(String, ModelIdentity, String)> = Vec::new();
    let catalog = nori_harness::recovery::route_catalog(home, active_agent);
    let offer_of = |agent: &str, value: &str| {
        catalog
            .iter()
            .find(|candidate| candidate.agent == agent)
            .and_then(|agent| agent.models.iter().find(|model| model.value == value))
            .and_then(|model| match &model.availability {
                Availability::Runnable => None,
                Availability::Unavailable { reason } => Some(reason.clone()),
            })
    };
    if let Some(option) = option
        && let acp::SessionConfigKind::Select(select) = &option.kind
    {
        let values: Vec<(&acp::SessionConfigSelectOption, Option<&str>)> = match &select.options {
            acp::SessionConfigSelectOptions::Ungrouped(values) => {
                values.iter().map(|value| (value, None)).collect()
            }
            acp::SessionConfigSelectOptions::Grouped(groups) => groups
                .iter()
                .flat_map(|group| {
                    group
                        .options
                        .iter()
                        .map(|value| (value, Some(group.name.as_str())))
                })
                .collect(),
            _ => Vec::new(),
        };
        for (value, provider) in values {
            let name = nori_harness::recovery::model_label(&value.value.0, &value.name, provider);
            let current = value.value == select.current_value;
            if !current
                && let Some(reason) = offer_of(active_agent.unwrap_or_default(), &value.value.0)
            {
                unavailable.push((
                    name.clone(),
                    ModelIdentity::of(&name, &value.value.0),
                    reason,
                ));
                continue;
            }
            add(
                &mut choices,
                &name,
                &value.value.0,
                ModelRoute::Session {
                    agent_name: active_agent.unwrap_or_default().to_owned(),
                    config_id: option.id.to_string(),
                    value: value.value.to_string(),
                    option_name: option.name.clone(),
                },
                current,
            );
        }
    }
    if !matches!(origin, ConfigPickerOrigin::Child(_)) {
        for agent in catalog
            .iter()
            .filter(|agent| Some(agent.agent.as_str()) != active_agent)
        {
            for model in &agent.models {
                let name = nori_harness::recovery::model_label(
                    &model.value,
                    &model.name,
                    Some(&agent.display_name),
                );
                match &model.availability {
                    Availability::Runnable => add(
                        &mut choices,
                        &name,
                        &model.value,
                        ModelRoute::Agent {
                            agent_name: agent.agent.clone(),
                            display_name: agent.display_name.clone(),
                            model: model.value.clone(),
                        },
                        false,
                    ),
                    Availability::Unavailable { reason } => {
                        unavailable.push((name.clone(), model.identity.clone(), reason.clone()))
                    }
                }
            }
        }
        // A native provider that has never listed its models is still where a model of its
        // family belongs; offer it under the id a gateway would have prefixed.
        for choice in &mut choices {
            let Some(family) = choice.identity.family() else {
                continue;
            };
            let Some(bare) = choice.routes.first().map(route_ref).and_then(|route| {
                let model = route.model?;
                Some(
                    model
                        .rsplit_once('/')
                        .map_or(model.clone(), |(_, bare)| bare.to_owned()),
                )
            }) else {
                continue;
            };
            for native in family.native_agents() {
                let Some(agent) = catalog.iter().find(|agent| agent.agent == *native) else {
                    continue;
                };
                if Some(*native) != active_agent
                    && agent.models.is_empty()
                    && !choice
                        .routes
                        .iter()
                        .any(|route| route_ref(route).agent == *native)
                {
                    choice.routes.push(ModelRoute::Agent {
                        agent_name: agent.agent.clone(),
                        display_name: agent.display_name.clone(),
                        model: bare.clone(),
                    });
                }
            }
        }
    }
    for (name, identity, reason) in unavailable {
        if !choices
            .iter()
            .any(|choice| choice.identity.is_same_model(&identity))
        {
            choices.push(ModelChoice {
                name,
                identity,
                routes: Vec::new(),
                current: false,
                unavailable: Some(reason),
            });
        }
    }
    for choice in &mut choices {
        let family = choice.identity.family();
        ranking.sort_same_model(&mut choice.routes, |route| (route_ref(route), family));
    }
    choices.sort_by_cached_key(|choice| {
        let best = choice
            .routes
            .first()
            .map(|route| ranking.key(&route_ref(route), choice.identity.family()));
        (choice.unavailable.is_some(), !choice.current, best)
    });
    distinguish_equal_names(&mut choices);
    choices
}

/// Two different models that an agent names the same way (`GPT 6`, a 6.1 and a 6.2) are told
/// apart by their version, then their variant. A name that is already unique is left as the
/// agent wrote it.
fn distinguish_equal_names(choices: &mut [ModelChoice]) {
    let names: Vec<String> = choices
        .iter()
        .map(|choice| choice.name.to_lowercase())
        .collect();
    for (index, choice) in choices.iter_mut().enumerate() {
        if names.iter().filter(|name| **name == names[index]).count() < 2 {
            continue;
        }
        if let Some(version) = &choice.identity.version
            && !choice.name.contains(version.as_str())
        {
            choice.name.push_str(&format!(" {version}"));
        }
        if let Some(variant) = &choice.identity.variant
            && !nori_harness::recovery::normalized(&choice.name).contains(variant)
        {
            let mut chars = variant.chars();
            if let Some(first) = chars.next() {
                choice.name.push_str(&format!(
                    " {}{}",
                    first.to_ascii_uppercase(),
                    chars.as_str()
                ));
            }
        }
    }
}

/// The ranking for the session on `config`, whose running model is `option`'s current value.
fn ranking_for(config: &Config, option: Option<&acp::SessionConfigOption>) -> RouteRanking {
    let active = option.and_then(|option| match &option.kind {
        acp::SessionConfigKind::Select(select) => Some(RouteRef::new(
            config.active_agent.clone(),
            Some(select.current_value.to_string()),
        )),
        _ => None,
    });
    RouteRanking::for_config(config, active)
}

pub(crate) fn params(
    config: &Config,
    active_agent: Option<&str>,
    option: Option<&acp::SessionConfigOption>,
    origin: ConfigPickerOrigin,
) -> SelectionViewParams {
    let ranking = ranking_for(config, option);
    let choices = collect_choices(&config.nori_home, active_agent, option, origin, &ranking);
    // Models are listed under the agent that runs them best (the first of their ranked
    // routes), agents in the order their best model ranks. The order inside a group is the
    // harness's ranking, untouched.
    let mut groups: Vec<(String, Vec<ModelChoice>)> = Vec::new();
    for choice in choices {
        let agent = match choice.routes.first() {
            Some(route) => agent_label(route),
            None => "Unavailable".to_owned(),
        };
        match groups.iter_mut().find(|(label, _)| *label == agent) {
            Some((_, members)) => members.push(choice),
            None => groups.push((agent, vec![choice])),
        }
    }
    let mut items = Vec::new();
    // How much of its tightest rate-limit window an account has left, as five cells and a figure.
    let room_text = |route: &ModelRoute| {
        let room = ranking.headroom()?.room_for(&route_ref(route))?;
        let filled = (room.percent_left / 20.0).round().clamp(0.0, 5.0) as usize;
        let meter = format!("{}{}", "▰".repeat(filled), "▱".repeat(5 - filled));
        Some(if room.is_spent() {
            format!("{meter} spent")
        } else {
            format!("{meter} {:.0}% left", room.percent_left)
        })
    };
    for (agent, members) in groups {
        items.push(SelectionItem {
            name: agent.clone(),
            is_header: true,
            ..Default::default()
        });
        for choice in members {
            // The account that can take the request comes first; the others follow with what
            // they have left, so the row shows where the capacity is.
            let others: Vec<String> = {
                let mut seen: Vec<String> = Vec::new();
                let mut labels: Vec<String> = Vec::new();
                for route in choice.routes.iter().skip(1) {
                    let label = agent_label(route);
                    if label != agent && !seen.contains(&label) {
                        labels.push(match room_text(route) {
                            Some(room) => format!("{label} {room}"),
                            None => label.clone(),
                        });
                        seen.push(label);
                    }
                }
                labels
            };
            let first_room = choice
                .routes
                .first()
                .filter(|_| !others.is_empty())
                .and_then(room_text);
            let name = choice.name;
            let value_name = name.clone();
            let routes = choice.routes;
            let selectable = !routes.is_empty();
            // Moving the cursor only highlights: nothing is switched, warmed or sent to an
            // agent until the row is chosen.
            let actions: Vec<crate::bottom_pane::SelectionAction> = if !selectable {
                Vec::new()
            } else {
                vec![Box::new(move |tx| {
                    tx.send(AppEvent::SelectModelRoutes {
                        routes: routes.clone(),
                        value_name: value_name.clone(),
                        origin,
                    })
                })]
            };
            items.push(SelectionItem {
                name: name.clone(),
                search_value: Some(format!("{name} {agent}")),
                description: match (choice.unavailable, others.is_empty()) {
                    (Some(reason), _) => Some(format!("unavailable: {reason}")),
                    (None, false) => Some(match first_room {
                        Some(room) => format!("{room} · also on {}", others.join(", ")),
                        None => format!("also on {}", others.join(", ")),
                    }),
                    (None, true) => None,
                },
                is_current: choice.current,
                actions,
                dismiss_on_select: selectable,
                ..Default::default()
            });
        }
    }
    let initial_selected_idx = items.iter().position(|item| item.is_current);
    SelectionViewParams {
        title: Some("Select model".to_owned()),
        items,
        initial_selected_idx,
        is_searchable: true,
        ..Default::default()
    }
    .picker()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tokio::sync::mpsc::unbounded_channel;

    fn config(home: &std::path::Path) -> Config {
        // The session's own agent is unnamed, as in the options below; an installed native agent
        // may still add its own route to a model of its family.
        Config {
            nori_home: home.to_path_buf(),
            active_agent: String::new(),
            ..Default::default()
        }
    }

    #[test]
    fn same_model_routes_share_one_clean_row_and_preserve_versions() {
        let option = acp::SessionConfigOption::select(
            "model",
            "Model",
            "local/other",
            vec![
                acp::SessionConfigSelectOption::new("openai/gpt-6.1-terra", "OpenAI/GPT 6.1 Terra"),
                acp::SessionConfigSelectOption::new(
                    "gateway/gpt-6.1-terra",
                    "Gateway/GPT 6.1 Terra",
                ),
                acp::SessionConfigSelectOption::new("gateway/gpt-6-terra", "Gateway/GPT 6 Terra"),
            ],
        )
        .category(acp::SessionConfigOptionCategory::Model);
        let home = tempfile::tempdir().unwrap();
        let params = params(
            &config(home.path()),
            None,
            Some(&option),
            ConfigPickerOrigin::Direct,
        );
        let rows: Vec<&SelectionItem> = params.items.iter().filter(|row| !row.is_header).collect();
        assert_eq!(
            rows.iter().map(|row| row.name.as_str()).collect::<Vec<_>>(),
            ["GPT 6.1 Terra", "GPT 6 Terra"]
        );
        let (tx, mut rx) = unbounded_channel();
        (rows[0].actions[0])(&crate::app_event_sender::AppEventSender::new(tx));
        let AppEvent::SelectModelRoutes { routes, .. } = rx.try_recv().unwrap() else {
            panic!("expected model routes");
        };
        let session_routes = routes
            .into_iter()
            .filter(|route| matches!(route, ModelRoute::Session { .. }))
            .collect::<Vec<_>>();
        assert_eq!(
            session_routes,
            vec![
                ModelRoute::Session {
                    agent_name: "".into(),
                    config_id: "model".into(),
                    value: "openai/gpt-6.1-terra".into(),
                    option_name: "Model".into()
                },
                ModelRoute::Session {
                    agent_name: "".into(),
                    config_id: "model".into(),
                    value: "gateway/gpt-6.1-terra".into(),
                    option_name: "Model".into()
                },
            ]
        );
    }

    #[test]
    fn current_choice_keeps_its_equivalent_routes_selectable() {
        let option = acp::SessionConfigOption::select(
            "model",
            "Model",
            "one/gpt-6",
            vec![
                acp::SessionConfigSelectOption::new("two/gpt-6", "GPT 6"),
                acp::SessionConfigSelectOption::new("one/gpt-6", "GPT 6"),
            ],
        )
        .category(acp::SessionConfigOptionCategory::Model);
        let home = tempfile::tempdir().unwrap();
        let params = params(
            &config(home.path()),
            None,
            Some(&option),
            ConfigPickerOrigin::Direct,
        );
        let rows: Vec<&SelectionItem> = params.items.iter().filter(|row| !row.is_header).collect();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].is_current);
        let (tx, mut rx) = unbounded_channel();
        (rows[0].actions[0])(&crate::app_event_sender::AppEventSender::new(tx));
        let AppEvent::SelectModelRoutes { routes, .. } = rx.try_recv().unwrap() else {
            panic!("expected model routes");
        };
        let session_routes = routes
            .iter()
            .filter(|route| matches!(route, ModelRoute::Session { .. }))
            .collect::<Vec<_>>();
        assert!(
            matches!(session_routes[0], ModelRoute::Session { value, .. } if value == "one/gpt-6")
        );
        assert!(
            matches!(session_routes[1], ModelRoute::Session { value, .. } if value == "two/gpt-6")
        );
    }

    #[test]
    fn unversioned_provider_aliases_share_a_floating_intent() {
        let mut choices = Vec::new();
        for agent in ["first", "second"] {
            add(
                &mut choices,
                "Default",
                "default",
                ModelRoute::Agent {
                    agent_name: agent.into(),
                    display_name: agent.into(),
                    model: "default".into(),
                },
                false,
            );
        }
        assert_eq!(choices.len(), 1);
        assert_eq!(choices[0].name, "Automatic");
        assert_eq!(choices[0].routes.len(), 2);
    }

    #[test]
    fn versioned_names_resolve_generic_ids_without_merging_other_versions() {
        let mut choices = Vec::new();
        for (name, value, agent) in [
            ("Sonnet 5.2", "sonnet", "first"),
            ("Sonnet 5.2", "sonnet-5-2", "second"),
            ("Sonnet 5.1", "sonnet-5-1", "third"),
            ("Sonnet 5.2 Fast", "sonnet-5-2-fast", "fourth"),
        ] {
            add(
                &mut choices,
                name,
                value,
                ModelRoute::Agent {
                    agent_name: agent.into(),
                    display_name: agent.into(),
                    model: value.into(),
                },
                false,
            );
        }
        assert_eq!(
            choices
                .iter()
                .map(|choice| (choice.name.as_str(), choice.routes.len()))
                .collect::<Vec<_>>(),
            [("Sonnet 5.2", 2), ("Sonnet 5.1", 1), ("Sonnet 5.2 Fast", 1)]
        );
    }

    #[test]
    fn explicit_variants_stay_distinct_even_when_labels_are_identical() {
        let mut choices = Vec::new();
        for value in ["gpt-6.1-terra", "gpt-6.1-terra-fast"] {
            add(
                &mut choices,
                "GPT 6.1 Terra",
                value,
                ModelRoute::Agent {
                    agent_name: "test".into(),
                    display_name: "Test".into(),
                    model: value.into(),
                },
                false,
            );
        }
        distinguish_equal_names(&mut choices);
        assert_eq!(
            choices
                .iter()
                .map(|choice| choice.name.as_str())
                .collect::<Vec<_>>(),
            ["GPT 6.1 Terra", "GPT 6.1 Terra Fast"]
        );
    }
}
