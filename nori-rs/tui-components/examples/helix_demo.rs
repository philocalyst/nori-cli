//! Prints the Helix picker engine at a few sizes and queries as plain text.
//!
//! `cargo run -p nori-tui-components --example helix_demo`

use std::collections::HashMap;
use std::time::Instant;

use nori_tui_components::Field;
use nori_tui_components::FilterKey;
use nori_tui_components::HelixConfig;
use nori_tui_components::HelixEvent;
use nori_tui_components::HelixItem;
use nori_tui_components::HelixState;
use nori_tui_components::Hit;
use nori_tui_components::ItemAction;
use nori_tui_components::Preview;
use nori_tui_components::PreviewAnchor;
use nori_tui_components::RowContext;
use nori_tui_components::Theme;
use nori_tui_components::highlight_indices;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::KeyCode;
use ratatui::crossterm::event::KeyEvent;
use ratatui::crossterm::event::KeyModifiers;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

struct Session {
    id: i64,
    title: &'static str,
    age: &'static str,
    branch: &'static str,
    agent: &'static str,
    model: &'static str,
    turns: i32,
}

impl HelixItem for Session {
    type Id = i64;

    fn id(&self) -> i64 {
        self.id
    }

    fn fields<'a>(&'a self, out: &mut Vec<Field<'a>>) {
        out.push(Field::new("title", self.title, 0));
        out.push(Field::new("agent", self.agent, 30));
        out.push(Field::new("branch", self.branch, 35));
        out.push(Field::new("model", self.model, 40));
    }

    fn row_height(&self) -> u16 {
        2
    }

    fn rows(&self, cx: &RowContext<'_>) -> Vec<Line<'static>> {
        let width = usize::from(cx.width);
        let age = format!(" {}", self.age);
        let room = width.saturating_sub(age.len() + 2);
        let (mut spans, used) = highlight_indices(
            self.title,
            cx.highlights.field("title"),
            Style::new(),
            cx.theme.accent_amber,
            room,
        );
        spans.insert(0, Span::styled("● ", cx.theme.accent_mint));
        spans.push(Span::raw(" ".repeat(room.saturating_sub(used))));
        spans.push(Span::styled(age, cx.theme.faint));
        let second = match cx.hit {
            Some(hit) => format!(
                "{} hits · {}",
                hit.count,
                hit.snippet.clone().unwrap_or_default()
            ),
            None => format!("{} · {} turns · {}", self.branch, self.turns, self.agent),
        };
        vec![
            Line::from(spans),
            Line::from(Span::styled(format!("  {second}"), cx.theme.faint)),
        ]
    }
}

const FILTERS: &[FilterKey] = &[
    FilterKey::new("agent", Some('@')),
    FilterKey::new("branch", None),
    FilterKey::new("model", None),
];

fn state() -> HelixState<Session> {
    let mut state = HelixState::new(HelixConfig {
        title: "Sessions".into(),
        placeholder: "search transcripts, titles, branches".into(),
        empty_text: "No sessions recorded for this project yet.".into(),
        no_match_text: "No session matches. Try fewer words or remove a filter.".into(),
        filter_keys: FILTERS,
        preview: true,
        multi: false,
        searchable: true,
        on_demand: false,
        numbered: false,
        subtitle: None,
    });
    state.set_items(vec![
        Session {
            id: 1,
            title: "Fence WakeTurn cancel delivery",
            age: "12m",
            branch: "instant-core",
            agent: "codex",
            model: "gpt-6",
            turns: 9,
        },
        Session {
            id: 2,
            title: "In-process host tools & fork IDs",
            age: "3h",
            branch: "main",
            agent: "claude-code",
            model: "opus-5",
            turns: 14,
        },
        Session {
            id: 3,
            title: "Primary scrollback SIGWINCH reflow",
            age: "6h",
            branch: "perf/instant-core",
            agent: "gemini",
            model: "gemini-4",
            turns: 6,
        },
        Session {
            id: 4,
            title: "OpenCode quota fallback & SSE",
            age: "1d",
            branch: "main",
            agent: "opencode",
            model: "deepseek-v4",
            turns: 22,
        },
        Session {
            id: 5,
            title: "Native Rust Pkl 0.33 module capture",
            age: "1d",
            branch: "pkl",
            agent: "claude-code",
            model: "haiku-5",
            turns: 5,
        },
        Session {
            id: 6,
            title: "Embed brush-core bash completions",
            age: "2d",
            branch: "brush",
            agent: "codex",
            model: "gpt-6",
            turns: 31,
        },
    ]);
    state
}

fn show(label: &str, state: &mut HelixState<Session>, width: u16, height: u16, theme: &Theme) {
    let area = Rect::new(0, 0, width, height);
    if let Some(request) = state.preview_request(area, Instant::now()) {
        let lines = vec![
            Line::from(Span::styled(
                "▌ Fence WakeTurn cancel delivery",
                theme.accent_violet,
            )),
            Line::from(""),
            Line::from("Wake delivery is now losslessly fenced. A simultaneous cancel no"),
            Line::from("longer drops a child verdict; peek_fenced(f) retains it."),
            Line::from(""),
            Line::from("• $ cargo test -p nori-harness -- wake_cancel"),
        ];
        let title = Line::from("Turn 4 · ~/Projects/nori-cli · Carbon");
        let preview: Preview<i64> = request.answer(0, title, lines, PreviewAnchor::FirstMatch);
        state.set_preview(preview);
    }
    let mut buf = Buffer::empty(area);
    state.render(area, &mut buf, theme, 0);
    println!("--- {label} ({width}x{height})");
    for y in 0..height {
        let row: String = (0..width).map(|x| buf[(x, y)].symbol()).collect();
        println!("{}", row.trim_end());
    }
}

fn press(
    state: &mut HelixState<Session>,
    code: KeyCode,
    modifiers: KeyModifiers,
) -> HelixEvent<i64> {
    state.on_key(KeyEvent::new(code, modifiers))
}

fn main() {
    let theme = Theme::default();
    let mut s = state();
    show("empty query", &mut s, 100, 14, &theme);
    s.set_query("fence @agent:codex");
    show("filter", &mut s, 100, 14, &theme);
    s.set_query("cancel");
    let _ = s.take_search_request();
    s.set_hits(
        2,
        HashMap::from([(
            3,
            Hit {
                score: 50,
                count: 4,
                snippet: Some("…peek_fenced(f) on cancel…".into()),
            },
        )]),
    );
    show("full-text hit", &mut s, 100, 14, &theme);
    s.set_query("zzzz");
    show("no match", &mut s, 100, 10, &theme);
    s.set_query("");
    show("narrow, no preview", &mut s, 60, 12, &theme);
    // Compose mode: Alt+M on the selected row asks the prompt for a message.
    let mut c = state().actions([ItemAction {
        name: "message",
        key: 'm',
        compose: Some("message"),
        plain: false,
    }]);
    c.set_query("");
    let _ = press(&mut c, KeyCode::Down, KeyModifiers::NONE);
    println!(
        "alt-m -> {:?}",
        press(&mut c, KeyCode::Char('m'), KeyModifiers::ALT)
    );
    for ch in "use peek_fenced here".chars() {
        let _ = press(&mut c, KeyCode::Char(ch), KeyModifiers::NONE);
    }
    show("compose", &mut c, 100, 8, &theme);
    println!(
        "enter -> {:?}",
        press(&mut c, KeyCode::Enter, KeyModifiers::NONE)
    );
    println!(
        "esc on empty -> {:?}",
        press(&mut c, KeyCode::Esc, KeyModifiers::NONE)
    );
}
