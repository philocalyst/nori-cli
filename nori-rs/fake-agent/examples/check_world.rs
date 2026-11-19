//! Load a world file, validate it, and say what is in it.
//!
//!     cargo run -p nori-fake-agent --example check_world -- fake-agent/worlds/comprehensive-chat.pkl [--json]

use nori_fake_agent::World;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .ok_or("usage: check_world <world.pkl|world.json> [--json]")?;
    let world = World::from_path(std::path::Path::new(&path))?;
    if args.next().as_deref() == Some("--json") {
        println!("{}", serde_json::to_string_pretty(&world)?);
        return Ok(());
    }
    println!("world {:?}: {}", world.name, world.description);
    println!(
        "default agent: {:?}, clock starts {}",
        world.default_agent, world.start
    );
    for agent in &world.agents {
        println!(
            "  agent {} ({}): {} models, {} modes, {} scenes",
            agent.slug,
            agent.title(),
            agent.models.len(),
            agent.modes.len(),
            agent.scenes.len()
        );
    }
    Ok(())
}
