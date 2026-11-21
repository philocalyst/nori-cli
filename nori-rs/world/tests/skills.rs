//! Skills (`nori:Skill`) through the real harness: a module in `$NORI_HOME/skills/` is listed by the
//! `skill` tool, and running it runs its program with world agents as its actors.
//!
//! World: `fake-agent/worlds/skills.pkl`.

mod support;

use nori_world::WorldSession;

const WORLD: &str = "skills.pkl";

const DUO: &str = r#"amends "nori:Skill"

name = "duo"
summary = "Two members answer at once; a chair reconciles."
parameters { ["question"] { description = "What the duo decides" } }
actors {
  ["a"] { agent = "member-a" }
  ["b"] { agent = "member-b" }
  ["chair"] { agent = "chair" }
}
steps {
  new Par {
    branches {
      new Send { to = "a"; message = "{{question}}"; bind = "first" }
      new Send { to = "b"; message = "{{question}}"; bind = "second" }
    }
  }
  new Send { to = "chair"; message = "A said: {{first}} / B said: {{second}}"; bind = "decision" }
}
result = "decision"
"#;

async fn boot() -> WorldSession {
    let mut options = support::options("lead");
    options.configure = Some(Box::new(|config: &mut nori_config::NoriConfig| {
        let skills = config.nori_home.join("skills");
        std::fs::create_dir_all(&skills).expect("skills directory");
        std::fs::write(skills.join("duo.pkl"), DUO).expect("the skill module");
    }));
    WorldSession::boot(support::world(WORLD), options)
        .await
        .unwrap_or_else(|error| panic!("boot: {error:#}"))
}

/// The `skill` tool lists the session's skills with their summary and parameters.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_skill_tool_lists_the_modules_skills() {
    let session = boot().await;
    let events = session.ask("list").await.expect("the turn ends");
    let text = events.root().text();
    assert!(
        text.contains("duo") && text.contains("Two members answer at once"),
        "{text}"
    );
}

/// Running a skill runs its program: both members answer the argument in parallel, and the chair's
/// reply to both answers is the skill's result.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_skill_runs_its_program_with_the_arguments_it_was_given() {
    let session = boot().await;
    let events = session
        .ask("Should we ship on Friday?")
        .await
        .expect("the turn ends");
    let text = events.root().text();
    assert!(
        text.contains(
            "The council decided: Decision from: A said: A thinks yes / B said: B thinks no"
        ),
        "{text}"
    );
}
