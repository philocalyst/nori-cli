//! A world through the real TUI, captured as text.
//!
//! Runs `nori --world <file>` on a pty with a `vt100` terminal (the `tui-pty-e2e` session), types
//! prompts, presses keys, waits for text or for time, and writes each frame to a file: the visible
//! screen, and the native scrollback behind it. No tmux; the frames are what a person would see,
//! row for row, and are meant to be diffed before and after a UI change.
//!
//!     cargo build --bin nori
//!     cargo run -p nori-world --example world_ui -- fake-agent/worlds/comprehensive-chat.pkl \
//!         --out frames --size 100x34 [--ask] "trim the parser input" /waitfor "14 passed" /frame done
//!
//! `--ask` makes the TUI ask for permission instead of approving everything (a debug `nori`
//! approves unless `NORI_TEST_REQUIRE_APPROVAL` is set). Arguments after the options are a script. A plain argument is typed as a prompt and sent with
//! Enter. `/wait MS`, `/waitfor TEXT` (up to 60 s), `/key enter|esc|up|down|tab|ctrl-c|ctrl-space|CHAR`,
//! `/type TEXT`, `/frame NAME` (writes `NN-NAME.screen.txt` and `NN-NAME.scrollback.txt`),
//! `/resize COLSxROWS`. A final frame is always written. `NORI_WORLD_CLOCK` (default `virtual`
//! skips the world's cadence; use `real` for streaming) sets the world's time.

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use tui_pty_e2e::Key;
use tui_pty_e2e::SessionConfig;
use tui_pty_e2e::TuiSession;

fn pump(session: &mut TuiSession, duration: Duration) -> anyhow::Result<()> {
    let until = Instant::now() + duration;
    while Instant::now() < until {
        session.poll()?;
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

/// The scrollback, oldest page first, without the live screen.
fn scrollback(session: &mut TuiSession, rows: usize) -> String {
    let mut pages: Vec<String> = Vec::new();
    for page in 1..200 {
        let contents = session.scrollback_contents(page * rows);
        if pages.last() == Some(&contents) {
            break;
        }
        pages.push(contents);
    }
    pages.reverse();
    pages.join("\n----- page -----\n")
}

fn write_frame(
    session: &mut TuiSession,
    out: &Path,
    index: usize,
    name: &str,
    rows: usize,
) -> anyhow::Result<()> {
    let stem = format!("{index:02}-{name}");
    std::fs::write(
        out.join(format!("{stem}.screen.txt")),
        session.screen_contents(),
    )?;
    std::fs::write(
        out.join(format!("{stem}.scrollback.txt")),
        scrollback(session, rows),
    )?;
    println!("wrote {stem}");
    Ok(())
}

fn size(text: &str) -> anyhow::Result<(u16, u16)> {
    let (cols, rows) = text.split_once('x').context("size is COLSxROWS")?;
    Ok((cols.parse()?, rows.parse()?))
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let world = PathBuf::from(
        args.next()
            .context("usage: world_ui <world> [--out DIR] [--size 100x34] <script>")?,
    );
    let world_file = nori_world::World::from_path(&world)?;
    let mut out = PathBuf::from("world-frames");
    let (mut cols, mut rows) = (100, 34);
    let mut script = Vec::new();
    let mut nori_args: Vec<String> = Vec::new();
    let mut ask = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--out" => out = PathBuf::from(args.next().context("--out DIR")?),
            // A debug `nori` approves every request unless this is set (see tui/src/lib.rs).
            "--ask" => ask = true,
            "--nori-arg" => nori_args.push(args.next().context("--nori-arg ARG")?),
            "--size" => (cols, rows) = size(&args.next().context("--size COLSxROWS")?)?,
            _ => script.push(arg),
        }
    }
    std::fs::create_dir_all(&out)?;
    let agent = world_file
        .default_agent()
        .map(|agent| agent.slug.clone())
        .context("the world defines no agents")?;
    let world = world.canonicalize()?;
    let mut config = SessionConfig::new()
        .with_agent(agent)
        .with_arg("--world")
        .with_arg(world.display().to_string())
        .with_agent_env(
            "NORI_WORLD_CLOCK",
            std::env::var("NORI_WORLD_CLOCK").unwrap_or_else(|_| "virtual".to_owned()),
        );
    if ask {
        config = config.with_agent_env("NORI_TEST_REQUIRE_APPROVAL", "1");
    }
    for arg in nori_args {
        config = config.with_arg(arg);
    }
    if let Some(policy) = &world_file.policy {
        config = config.with_policy_pkl(policy.clone());
    }
    let mut session = TuiSession::spawn_with_config(rows, cols, config)
        .map_err(|error| anyhow::anyhow!("{error:#}"))?;
    session
        .wait_for_text("INS", Duration::from_secs(30))
        .map_err(|error| anyhow::anyhow!("the TUI did not start: {error}"))?;

    let mut frames = 0;
    for step in &script {
        match step.as_str() {
            other if other.starts_with("/wait ") => pump(
                &mut session,
                Duration::from_millis(other[6..].trim().parse()?),
            )?,
            other if other.starts_with("/waitfor ") => {
                let needle = other[9..].to_owned();
                if let Err(error) = session.wait_for_text(&needle, Duration::from_secs(60)) {
                    println!("did not see {needle:?}: {error}");
                }
            }
            other if other.starts_with("/key ") => {
                let key = match other[5..].trim() {
                    "enter" => Key::Enter,
                    "esc" => Key::Escape,
                    "up" => Key::Up,
                    "down" => Key::Down,
                    "tab" => Key::Tab,
                    "ctrl-c" => Key::Ctrl('c'),
                    "ctrl-space" => Key::Ctrl(' '),
                    key if key.chars().count() == 1 => Key::Char(key.chars().next().unwrap_or(' ')),
                    key => anyhow::bail!("unknown key {key:?}"),
                };
                session.send_key(key)?;
                pump(&mut session, Duration::from_millis(300))?;
            }
            other if other.starts_with("/type ") => session.send_str(&other[6..])?,
            other if other.starts_with("/frame ") => {
                pump(&mut session, Duration::from_millis(300))?;
                frames += 1;
                write_frame(
                    &mut session,
                    &out,
                    frames,
                    other[7..].trim(),
                    usize::from(rows),
                )?;
            }
            other if other.starts_with("/resize ") => {
                let (cols, rows) = size(other[8..].trim())?;
                session
                    .resize(rows, cols)
                    .map_err(|error| anyhow::anyhow!("{error:#}"))?;
                pump(&mut session, Duration::from_millis(500))?;
            }
            prompt => {
                session.send_str(prompt)?;
                pump(&mut session, Duration::from_millis(200))?;
                session.send_key(Key::Enter)?;
                pump(&mut session, Duration::from_millis(300))?;
            }
        }
    }
    pump(&mut session, Duration::from_millis(500))?;
    frames += 1;
    write_frame(&mut session, &out, frames, "final", usize::from(rows))?;
    println!("{}", session.screen_contents());
    Ok(())
}
