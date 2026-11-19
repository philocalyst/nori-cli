//! Evaluate any Pkl module with the native evaluator and the embedded `nori:*` schemas,
//! printing the result as JSON: `cargo run -p nori-conduit --example pkl_eval -- <file.pkl>`.

use nori_conduit::program_evaluator_options;
use nori_pkl::native::evaluate_path;

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: pkl_eval <file.pkl>");
        std::process::exit(2);
    };
    match evaluate_path::<serde_json::Value>(
        std::path::Path::new(&path),
        &program_evaluator_options(),
    ) {
        Ok(value) => println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_default()
        ),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
