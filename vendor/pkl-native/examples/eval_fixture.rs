//! Evaluate with the fixed environment from Apple's LanguageSnippetTestsEngine.
//! This example is experimental test infrastructure, not the production Nori API.
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let (text, path) = match args.as_slice() {
        [path] => (false, path),
        [flag, path] if flag == "--text" => (true, path),
        _ => {
            eprintln!("usage: eval_fixture [--text] FILE");
            return ExitCode::from(2);
        }
    };
    let builder = pklr::EvaluatorBuilder::new()
        .environment_variables([
            ("NAME1".into(), "value1".into()),
            ("NAME2".into(), "value2".into()),
            ("/foo/bar".into(), "foobar".into()),
            ("foo bar".into(), "foo bar".into()),
            ("file:///foo/bar".into(), "file:///foo/bar".into()),
        ])
        .external_properties([
            ("name1".into(), "value1".into()),
            ("name2".into(), "value2".into()),
            ("/foo/bar".into(), "foobar".into()),
        ]);
    let result = if text {
        builder.eval_to_text(Path::new(path))
    } else {
        builder.eval_to_json(Path::new(path)).map(|value| {
            // serde_json::Value always serializes successfully.
            format!("{value}\n")
        })
    };
    match result {
        Ok(output) => match std::io::stdout().write_all(output.as_bytes()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{error}");
                ExitCode::from(3)
            }
        },
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
