//! Prints the `/resume` picker and the actor panel over fixture data.

use nori_tui::storybook::actors_specimen;
use nori_tui::storybook::choice_specimen;
use nori_tui::storybook::resume_specimen;

fn main() {
    for (width, height, query) in [
        (120, 20, ""),
        (120, 20, "cancel"),
        (120, 20, "fence @agent:codex"),
        (70, 14, ""),
    ] {
        println!("--- /resume {width}x{height} query={query:?}");
        println!("{}", resume_specimen(width, height, query));
    }
    for (width, height, query, selected) in [
        (120, 16, "", 1),
        (120, 16, "%running", 2),
        (120, 16, "fence", 1),
        (60, 12, "", 3),
    ] {
        println!("--- actors {width}x{height} query={query:?}");
        println!("{}", actors_specimen(width, height, query, selected));
    }
    println!("--- choices 60");
    println!("{}", choice_specimen(60));
}
