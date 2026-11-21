//! The projection of commands into actions, and an invocation's argument into parameters.

use std::collections::BTreeMap;
use std::path::PathBuf;

use nori_actions::Action;
use nori_actions::ActionId;
use nori_actions::Category;
use nori_actions::Choice;
use nori_actions::ChoiceSource;
use nori_actions::Effect;
use nori_actions::Invocation;
use nori_actions::Param;
use nori_actions::Requires;
use nori_actions::Source;
use nori_pkl::catalog::Catalog;
use nori_pkl::catalog::Loaded;
use nori_pkl::catalog::Scope;
use nori_pkl::kinds::ChoiceFrom;
use nori_pkl::kinds::Command;
use nori_pkl::kinds::CommandCategory;
use nori_pkl::kinds::CommandEffect;
use nori_pkl::kinds::CommandRequires;
use nori_pkl::kinds::Parameter;
use nori_pkl::kinds::PluginDef;
use pretty_assertions::assert_eq;

use super::actions::project;
use super::invocation::resolve;
use super::types::PluginError;
use super::types::PluginInfo;

fn parameter(required: bool, default: Option<&str>) -> Parameter {
    Parameter {
        description: String::new(),
        default: default.map(str::to_owned),
        required,
        choices: Vec::new(),
        from: None,
    }
}

fn command(parameters: Vec<(&str, Parameter)>) -> Command {
    Command {
        description: "Does the thing".to_owned(),
        title: None,
        names: Vec::new(),
        keywords: Vec::new(),
        category: CommandCategory::Plugin,
        effect: CommandEffect::Now,
        requires: CommandRequires::default(),
        parameters: parameters
            .into_iter()
            .map(|(name, parameter)| (name.to_owned(), parameter))
            .collect(),
        operations: Vec::new(),
    }
}

fn catalog(commands: Vec<(&str, Command)>) -> Catalog {
    Catalog {
        plugins: vec![Loaded {
            name: "rex".to_owned(),
            path: PathBuf::from("/home/plugins/rex.pkl"),
            scope: Scope::User,
            value: PluginDef {
                name: "rex".to_owned(),
                description: String::new(),
                commands: commands
                    .into_iter()
                    .map(|(name, command)| (name.to_owned(), command))
                    .collect(),
            },
        }],
        ..Catalog::default()
    }
}

/// The one action of a catalog with one command.
fn only(command: Command) -> Action {
    let actions = project(&catalog(vec![("go", command)]));
    assert_eq!(actions.len(), 1);
    actions[0].clone()
}

#[test]
fn a_command_without_parameters_is_an_action_that_takes_nothing() {
    assert_eq!(
        only(command(vec![])),
        Action {
            id: ActionId::new("rex.go"),
            title: "Does the thing".to_owned(),
            summary: "Does the thing".to_owned(),
            names: vec!["rex-go".to_owned()],
            keywords: Vec::new(),
            category: Category::Plugin,
            param: Param::None,
            effect: Effect::Now,
            requires: Requires::default(),
            keys: Vec::new(),
            source: Source::Plugin("rex".to_owned()),
        }
    );
}

#[test]
fn the_commands_own_fields_replace_the_defaults() {
    let mut moving = command(vec![]);
    moving.title = Some("Move it".to_owned());
    moving.names = vec!["move".to_owned(), "mv".to_owned()];
    moving.keywords = vec!["remote".to_owned()];
    moving.category = CommandCategory::Machine;
    moving.effect = CommandEffect::OnSend;
    moving.requires = CommandRequires {
        idle: true,
        main: true,
        local: false,
        cloud: true,
    };

    let action = only(moving);

    assert_eq!(action.title, "Move it");
    assert_eq!(action.summary, "Does the thing");
    assert_eq!(action.names, ["move", "mv"]);
    assert_eq!(action.keywords, ["remote"]);
    assert_eq!(action.category, Category::Machine);
    assert_eq!(action.effect, Effect::OnSend);
    assert_eq!(
        action.requires,
        Requires {
            idle: true,
            main: true,
            local: false,
            cloud: true,
        }
    );
}

#[test]
fn a_command_with_neither_title_nor_description_is_titled_by_its_id() {
    let mut bare = command(vec![]);
    bare.description = String::new();

    assert_eq!(only(bare).title, "rex.go");
}

#[test]
fn one_parameter_with_choices_is_a_fixed_choice() {
    let mut tone = parameter(true, None);
    tone.choices = vec!["soft".to_owned(), "loud".to_owned()];

    assert_eq!(
        only(command(vec![("tone", tone)])).param,
        Param::Choice(ChoiceSource::Fixed(vec![
            Choice {
                value: "soft".to_owned(),
                label: "soft".to_owned(),
                detail: None
            },
            Choice {
                value: "loud".to_owned(),
                label: "loud".to_owned(),
                detail: None
            },
        ]))
    );
}

#[test]
fn one_parameter_with_a_source_is_a_choice_from_it() {
    for (from, source) in [
        (ChoiceFrom::Machines, ChoiceSource::Machines),
        (ChoiceFrom::Models, ChoiceSource::Models),
        (ChoiceFrom::Efforts, ChoiceSource::Efforts),
        (ChoiceFrom::Modes, ChoiceSource::Modes),
        (ChoiceFrom::Agents, ChoiceSource::Agents),
        (ChoiceFrom::Skills, ChoiceSource::Skills),
        (ChoiceFrom::Files, ChoiceSource::Files),
        (ChoiceFrom::Sessions, ChoiceSource::Sessions),
    ] {
        let mut machine = parameter(true, None);
        machine.from = Some(from);

        assert_eq!(
            only(command(vec![("machine", machine)])).param,
            Param::Choice(source)
        );
    }
}

#[test]
fn one_plain_parameter_is_text_with_its_description_as_the_placeholder() {
    let mut text = parameter(true, None);
    text.description = "What to say".to_owned();
    let mut optional = parameter(true, Some("hi"));
    optional.description = String::new();

    assert_eq!(
        only(command(vec![("text", text)])).param,
        Param::Text {
            placeholder: "What to say".to_owned(),
            required: true
        }
    );
    // With a default nobody has to type it, and with no description the name stands in.
    assert_eq!(
        only(command(vec![("greeting", optional)])).param,
        Param::Text {
            placeholder: "greeting".to_owned(),
            required: false
        }
    );
}

#[test]
fn several_parameters_are_text_of_pairs() {
    assert_eq!(
        only(command(vec![
            ("machine", parameter(true, None)),
            ("note", parameter(false, None)),
        ]))
        .param,
        Param::Text {
            placeholder: "machine=… note=…".to_owned(),
            required: true
        }
    );
    assert_eq!(
        only(command(vec![
            ("a", parameter(false, None)),
            ("b", parameter(true, Some("x"))),
        ]))
        .param,
        Param::Text {
            placeholder: "a=… b=…".to_owned(),
            required: false
        }
    );
}

#[test]
fn every_command_of_every_plugin_is_an_action_in_order() {
    let ids: Vec<String> = project(&catalog(vec![
        ("where", command(vec![])),
        ("move", command(vec![])),
    ]))
    .iter()
    .map(|action| action.id.to_string())
    .collect();

    assert_eq!(ids, ["rex.move", "rex.where"]);
}

fn plugins() -> Vec<PluginInfo> {
    PluginInfo::of(&catalog(vec![
        ("none", command(vec![])),
        ("say", command(vec![("text", parameter(true, None))])),
        (
            "send",
            command(vec![
                ("machine", parameter(true, None)),
                ("note", parameter(false, None)),
                ("tone", parameter(true, None)),
                ("kind", parameter(true, Some("plain"))),
            ]),
        ),
    ]))
}

fn run_of(
    id: &str,
    arg: Option<&str>,
) -> Result<(String, String, BTreeMap<String, String>), PluginError> {
    resolve(
        &plugins(),
        &Invocation::new(&ActionId::new(id), arg.map(str::to_owned)),
    )
}

fn arguments(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

#[test]
fn an_invocation_names_its_plugin_and_command() {
    assert_eq!(
        run_of("rex.none", None),
        Ok(("rex".to_owned(), "none".to_owned(), BTreeMap::new()))
    );
    assert_eq!(
        run_of("rex.none", Some("  ")),
        Ok(("rex".to_owned(), "none".to_owned(), BTreeMap::new()))
    );
}

#[test]
fn an_unknown_id_lists_the_known_ones() {
    let error = run_of("rex.nope", None).unwrap_err();

    assert_eq!(
        error.to_string(),
        "there is no plugin action `rex.nope`; the plugin actions are: rex.none, rex.say, rex.send"
    );
    assert!(matches!(
        run_of("model.set", None),
        Err(PluginError::UnknownAction { .. })
    ));
    assert!(matches!(
        run_of("nodot", None),
        Err(PluginError::UnknownAction { .. })
    ));
}

#[test]
fn a_command_without_parameters_refuses_an_argument() {
    assert_eq!(
        run_of("rex.none", Some("gpu")).unwrap_err().to_string(),
        "`rex none`: it takes no argument, but was given `gpu`"
    );
}

#[test]
fn one_parameter_takes_the_whole_argument() {
    assert_eq!(
        run_of("rex.say", Some(" hello there = you ")),
        Ok((
            "rex".to_owned(),
            "say".to_owned(),
            arguments(&[("text", "hello there = you")])
        ))
    );
}

#[test]
fn several_parameters_take_pairs_and_quoted_values() {
    assert_eq!(
        run_of(
            "rex.send",
            Some(r#"machine=gpu note="two words" tone=soft"#)
        ),
        Ok((
            "rex".to_owned(),
            "send".to_owned(),
            arguments(&[("machine", "gpu"), ("note", "two words"), ("tone", "soft")])
        ))
    );
}

#[test]
fn a_bare_value_fills_the_first_required_parameter_the_pairs_left() {
    assert_eq!(
        run_of("rex.send", Some("gpu tone=soft")),
        Ok((
            "rex".to_owned(),
            "send".to_owned(),
            arguments(&[("machine", "gpu"), ("tone", "soft")])
        ))
    );
    // Required parameters are filled in name order; one with a default is not required.
    assert_eq!(
        run_of("rex.send", Some("soft gpu")),
        Ok((
            "rex".to_owned(),
            "send".to_owned(),
            arguments(&[("machine", "soft"), ("tone", "gpu")])
        ))
    );
}

#[test]
fn a_bad_argument_is_named() {
    assert_eq!(
        run_of("rex.send", Some("machin=gpu"))
            .unwrap_err()
            .to_string(),
        "`rex send` has no argument `machin`; its arguments are: kind, machine, note, tone"
    );
    assert_eq!(
        run_of("rex.send", Some("machine=a machine=b"))
            .unwrap_err()
            .to_string(),
        "`rex send`: `machine` is given twice"
    );
    assert_eq!(
        run_of("rex.send", Some("a b c")).unwrap_err().to_string(),
        "`rex send`: no required parameter is left for `c`"
    );
    assert_eq!(
        run_of("rex.send", Some(r#"note="oops"#))
            .unwrap_err()
            .to_string(),
        "`rex send`: `note=\"oops` has an unclosed quote"
    );
}
