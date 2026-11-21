use super::*;
use crate::Theme;
use insta::assert_snapshot;
use pretty_assertions::assert_eq;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;

struct Row {
    id: u8,
    label: &'static str,
    section: Option<&'static str>,
    height: u16,
}

impl HelixItem for Row {
    type Id = u8;

    fn id(&self) -> Self::Id {
        self.id
    }

    fn fields<'a>(&'a self, out: &mut Vec<Field<'a>>) {
        out.push(Field::new("title", self.label, 0));
    }

    fn section(&self) -> Option<&str> {
        self.section
    }

    fn row_height(&self) -> u16 {
        self.height
    }

    fn rows(&self, _cx: &RowContext<'_>) -> Vec<Line<'static>> {
        vec![Line::from(self.label); usize::from(self.height.max(1))]
    }
}

fn state(preview: bool) -> HelixState<Row> {
    HelixState::new(HelixConfig {
        title: "Credentials".into(),
        placeholder: "search".into(),
        empty_text: "No entries".into(),
        no_match_text: "No match".into(),
        filter_keys: &[],
        preview,
        multi: false,
        searchable: true,
        on_demand: false,
        numbered: false,
        subtitle: None,
    })
}

fn plain_rows(buffer: &Buffer) -> String {
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn masked_compose_prompt_shows_dots_instead_of_the_credential() {
    let mut state = state(false);
    state.begin_compose(
        "login",
        "sign in",
        "example".into(),
        "sensitive-value".into(),
        1,
    );
    state.mask_compose();

    let area = Rect::new(0, 0, 48, 7);
    let mut buffer = Buffer::empty(area);
    state.render(area, &mut buffer, &Theme::default(), 0);
    let rendered = plain_rows(&buffer);

    assert!(!rendered.contains("sensitive-value"));
    assert_snapshot!(rendered);
}

#[test]
fn wanted_height_accounts_for_sections_rows_and_preview_minimum() {
    let mut state = state(true);
    state.set_items(vec![
        Row {
            id: 1,
            label: "one",
            section: Some("First"),
            height: 2,
        },
        Row {
            id: 2,
            label: "two",
            section: Some("First"),
            height: 1,
        },
        Row {
            id: 3,
            label: "three",
            section: Some("Second"),
            height: 1,
        },
    ]);

    // Two section headings + four row lines + four rows of frame chrome.
    assert_eq!(state.height_wanted(60), 10);
    // A wide preview needs at least eight body rows, even when the list is shorter.
    assert_eq!(state.height_wanted(78), 12);
}
