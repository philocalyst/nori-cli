//! Every showcase world is a valid world: it evaluates, fits the serde model, and validates.
//! A schema change that breaks a world, or a world that drifts from the schema, fails here
//! before anyone opens it in a TUI.

#[test]
fn every_showcase_world_loads() {
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../fake-agent/worlds");
    let mut loaded = Vec::new();
    let mut failed = Vec::new();
    for entry in std::fs::read_dir(&directory).expect("the worlds directory") {
        let path = entry.expect("a directory entry").path();
        if path.extension().is_none_or(|extension| extension != "pkl") {
            continue;
        }
        match nori_world::World::from_path(&path) {
            Ok(world) => loaded.push(world.name),
            Err(error) => failed.push(error.to_string()),
        }
    }
    assert!(
        failed.is_empty(),
        "worlds that do not load:\n{}",
        failed.join("\n")
    );
    assert!(loaded.len() >= 12, "only found {loaded:?}");
}
