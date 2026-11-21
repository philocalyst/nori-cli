//! Nori's built-in skills through the real harness: `impact` and `council` are listed by the
//! `skill` tool without any module in `$NORI_HOME`, run their programs with world agents as the
//! actors, and give way to a module of the same name in `$NORI_HOME/skills/`.
//!
//! World: `fake-agent/worlds/builtin-skills.pkl`.

mod support;

use nori_world::WorldSession;

const WORLD: &str = "builtin-skills.pkl";

async fn boot(shadow: Option<&'static str>) -> WorldSession {
    let mut options = support::options("lead");
    options.configure = Some(Box::new(move |config: &mut nori_config::NoriConfig| {
        if let Some(source) = shadow {
            let skills = config.nori_home.join("skills");
            std::fs::create_dir_all(&skills)
                .unwrap_or_else(|error| panic!("skills directory: {error}"));
            std::fs::write(skills.join("council.pkl"), source)
                .unwrap_or_else(|error| panic!("the skill module: {error}"));
        }
    }));
    WorldSession::boot(support::world(WORLD), options)
        .await
        .unwrap_or_else(|error| panic!("boot: {error:#}"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_skill_tool_lists_the_built_in_skills() {
    let session = boot(None).await;
    let events = session.ask("list").await.expect("the turn ends");
    let text = events.root().text();
    assert!(
        text.contains("impact") && text.contains("council") && text.contains("declarations"),
        "{text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn impact_scopes_then_assesses() {
    let session = boot(None).await;
    let events = session
        .ask("impact parse_config")
        .await
        .expect("the turn ends");
    let text = events.root().text();
    assert!(
        text.contains(
            "Impact report: ## Impact\nMedium: 2 direct users [references rust:app::parse_config]"
        ),
        "{text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn council_members_answer_in_parallel_and_the_chair_reconciles() {
    let session = boot(None).await;
    let events = session
        .ask("council Is this migration safe?")
        .await
        .expect("the turn ends");
    let text = events.root().text();
    assert!(
        text.contains("The council reported: Chair: agreed, ship it.")
            && text.contains("Member 1: looks fine. Score: 8/10")
            && text.contains("Member 2: one nit. Score: 7/10")
            && !text.contains("Member 3"),
        "{text}"
    );
}

const MINE: &str = r#"amends "nori:Skill"

name = "council"
summary = "My own council."
parameters { ["request"] { description = "What to decide" } }
actors { ["only"] { agent = "codex" } }
steps { new Send { to = "only"; message = "{{request}}"; bind = "answer" } }
result = "answer"
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_users_skill_replaces_the_built_in_one_of_the_same_name() {
    let session = boot(Some(MINE)).await;
    let events = session.ask("list").await.expect("the turn ends");
    let text = events.root().text();
    assert!(
        text.contains("My own council.") && !text.contains("independent"),
        "{text}"
    );
}
