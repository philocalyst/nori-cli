//! Tests that the incremental collector is the one-shot renderer, delivered in pieces.

use super::*;
use pretty_assertions::assert_eq;
use proptest::prelude::*;

fn plain(lines: &[Line<'static>]) -> Vec<String> {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect()
}

/// Each line's text and styles, ignoring how the text is cut into spans.
fn styled(lines: &[Line<'static>]) -> Vec<Vec<(String, ratatui::style::Style)>> {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .filter(|span| !span.content.is_empty())
                .map(|span| (span.content.to_string(), line.style.patch(span.style)))
                .collect()
        })
        .collect()
}

fn one_shot_lines(source: &str, width: Option<usize>) -> Vec<Line<'static>> {
    let mut source = source.to_owned();
    if !source.is_empty() && !source.ends_with('\n') {
        source.push('\n');
    }
    let mut lines = Vec::new();
    crate::markdown::append_markdown_with_cwd(&source, width, None, &mut lines);
    lines
}

/// Lines of rendering the whole message at once, as the stream's finalize does.
fn one_shot(source: &str, width: Option<usize>) -> Vec<String> {
    plain(&one_shot_lines(source, width))
}

fn streamed_lines(
    pieces: &[&str],
    advance_every: usize,
    width: Option<usize>,
) -> Vec<Line<'static>> {
    let mut collector = MarkdownStreamCollector::new(width);
    let mut out = Vec::new();
    for (index, piece) in pieces.iter().enumerate() {
        collector.push_delta(piece);
        if (index + 1) % advance_every == 0 {
            out.extend(collector.advance());
        }
    }
    out.extend(collector.finalize_and_drain().0);
    out
}

/// Stream `pieces`, advancing after every `advance_every`-th one, and return all lines handed out.
fn streamed(pieces: &[&str], advance_every: usize, width: Option<usize>) -> Vec<String> {
    plain(&streamed_lines(pieces, advance_every, width))
}

/// Split `text` into pieces at the given char indices.
fn split_at_chars(text: &str, cuts: &[usize]) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut cuts: Vec<usize> = cuts.iter().map(|cut| cut % (chars.len() + 1)).collect();
    cuts.sort_unstable();
    cuts.dedup();
    let mut pieces = Vec::new();
    let mut start = 0;
    for cut in cuts {
        pieces.push(chars[start..cut].iter().collect::<String>());
        start = cut;
    }
    pieces.push(chars[start..].iter().collect());
    pieces
}

fn fragment() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("Plain paragraph with some words.".to_string()),
        Just("A paragraph that is long enough to wrap when the width is narrow, with `code`, **bold** and *emphasis*.".to_string()),
        Just("# Heading one".to_string()),
        Just("## Heading two with `code`".to_string()),
        Just("- bullet one\n- bullet two\n  - nested bullet\n- bullet three".to_string()),
        Just("1. first\n2. second\n\n3. third after a blank line".to_string()),
        Just("1. loose item\n\n   with its own paragraph\n\n2. next item".to_string()),
        Just("> quoted line\n> second quoted line".to_string()),
        Just("> outer\n>> inner\n> - list in quote".to_string()),
        Just("```rust\nfn main() {\n    println!(\"hi\");\n}\n```".to_string()),
        Just("```\nplain fence\n\nwith a blank line\n```".to_string()),
        Just("~~~python\nx = 1\n\n\ny = \"two\"\n~~~".to_string()),
        Just("```unknownlang\na\n\nb\n\n```".to_string()),
        Just("````\n```\ninner fence\n```\n````".to_string()),
        Just("  ```js\n  let x = 1;\n    nested\n  ```".to_string()),
        Just("```rust\n/* a comment\nspanning lines */\nlet s = \"str\";\n```".to_string()),
        Just("```\n| A | B |\n| --- | --- |\n| 1 | 2 |\n```".to_string()),
        Just("```rust\nfn trailing_blanks() {}\n\n\n```".to_string()),
        Just("```text\nan unclosed fence swallows the rest".to_string()),
        Just("    indented code\n\n    still code".to_string()),
        Just("| A | B |\n| --- | --- |\n| 1 | 2 |\n| 3 | 4 |".to_string()),
        Just("[a link](https://example.com/path) and <b>inline html</b>".to_string()),
        Just("[local](src/main.rs) link".to_string()),
        Just("漢字とカナ mixed with ASCII and 😀 emoji".to_string()),
        Just("combining e\u{301} marks and a\u{304}".to_string()),
        Just("line with hard break  \nnext line".to_string()),
        Just("***".to_string()),
        Just("- [ ] task item\n- [x] done item".to_string()),
        Just("text with | a pipe but no table".to_string()),
        Just("a | b\nplain after a pipe line".to_string()),
    ]
}

fn document() -> impl Strategy<Value = String> {
    prop::collection::vec(
        (
            fragment(),
            prop_oneof![Just("\n"), Just("\n\n"), Just("\n\n\n")],
        ),
        1..8,
    )
    .prop_map(|parts| {
        parts
            .into_iter()
            .map(|(fragment, separator)| format!("{fragment}{separator}"))
            .collect::<String>()
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn any_split_of_any_document_equals_the_one_shot_render(
        doc in document(),
        cuts in prop::collection::vec(0usize..2000, 0..40),
        advance_every in 1usize..5,
        narrow in any::<bool>(),
    ) {
        let width = narrow.then_some(24);
        let pieces = split_at_chars(&doc, &cuts);
        let refs: Vec<&str> = pieces.iter().map(String::as_str).collect();
        prop_assert_eq!(streamed(&refs, advance_every, width), one_shot(&doc, width));
    }

    #[test]
    fn streamed_styles_equal_the_one_shot_render(
        doc in document(),
        cuts in prop::collection::vec(0usize..2000, 0..20),
    ) {
        let pieces = split_at_chars(&doc, &cuts);
        let refs: Vec<&str> = pieces.iter().map(String::as_str).collect();
        prop_assert_eq!(styled(&streamed_lines(&refs, 1, None)), styled(&one_shot_lines(&doc, None)));
    }

    #[test]
    fn one_character_at_a_time_equals_the_one_shot_render(doc in document()) {
        let pieces: Vec<String> = doc.chars().map(String::from).collect();
        let refs: Vec<&str> = pieces.iter().map(String::as_str).collect();
        prop_assert_eq!(streamed(&refs, 1, None), one_shot(&doc, None));
    }

    #[test]
    fn a_message_without_a_trailing_newline_still_matches(
        doc in document(),
        cuts in prop::collection::vec(0usize..2000, 0..10),
    ) {
        let doc = doc.trim_end().to_string();
        let pieces = split_at_chars(&doc, &cuts);
        let refs: Vec<&str> = pieces.iter().map(String::as_str).collect();
        prop_assert_eq!(streamed(&refs, 1, None), one_shot(&doc, None));
    }
}

#[test]
fn bytes_of_a_multibyte_document_split_between_every_char_match() {
    let doc = "漢字 and é and 😀\n\n- 一\n- 二\n";
    let pieces: Vec<String> = doc.chars().map(String::from).collect();
    let refs: Vec<&str> = pieces.iter().map(String::as_str).collect();
    assert_eq!(streamed(&refs, 1, Some(10)), one_shot(doc, Some(10)));
}

#[test]
fn the_line_being_typed_is_live_before_its_newline() {
    let mut collector = MarkdownStreamCollector::new(None);
    collector.push_delta("Hello wor");
    let committed = collector.advance();
    assert_eq!(plain(&committed), Vec::<String>::new());
    assert_eq!(plain(collector.live_tail()), vec!["Hello wor".to_string()]);

    collector.push_delta("ld\nsecond li");
    let committed = collector.advance();
    assert_eq!(plain(&committed), vec!["Hello world".to_string()]);
    assert_eq!(plain(collector.live_tail()), vec!["second li".to_string()]);

    collector.push_delta("ne\n");
    let committed = collector.advance();
    assert_eq!(plain(&committed), vec!["second line".to_string()]);
    assert_eq!(plain(collector.live_tail()), Vec::<String>::new());
}

#[test]
fn a_two_thousand_line_burst_is_committed_by_a_single_advance() {
    let mut collector = MarkdownStreamCollector::new(None);
    let burst: String = (0..2000).map(|line| format!("line {line}\n")).collect();
    for piece in burst.as_bytes().chunks(64) {
        collector.push_delta(std::str::from_utf8(piece).expect("ascii"));
    }
    let committed = collector.advance();
    assert_eq!(committed.len(), 2000);
    assert_eq!(plain(&committed)[1999], "line 1999");
}

#[test]
fn a_fenced_burst_is_highlighted_over_bounded_frames_and_comes_out_whole() {
    let (doc, _) = rust_fence(2_000);
    let mut collector = MarkdownStreamCollector::new(Some(80));
    collector.push_delta(&doc);

    let mut frames = Vec::new();
    while collector.is_dirty() {
        frames.push(collector.advance());
    }
    let mut out: Vec<Line<'static>> = frames.iter().flatten().cloned().collect();
    out.extend(collector.finalize_and_drain().0);

    assert!(frames.len() > 2, "{} frames", frames.len());
    assert!(
        frames
            .iter()
            .all(|lines| lines.len() <= fence::MAX_LINES_PER_ADVANCE + 4),
        "{:?}",
        frames.iter().map(Vec::len).collect::<Vec<_>>()
    );
    assert_eq!(styled(&out), styled(&one_shot_lines(&doc, Some(80))));
}

fn paragraphs(count: usize) -> String {
    (0..count)
        .map(|index| {
            format!("Paragraph {index} has a few words in it, some `code` and **bold** text.\n\n")
        })
        .collect()
}

#[test]
fn streaming_a_200_kib_message_parses_a_bounded_multiple_of_its_size() {
    let doc = paragraphs(3000);
    assert!(doc.len() > 200 * 1024, "{}", doc.len());
    let mut collector = MarkdownStreamCollector::new(Some(80));
    let mut advances = 0;
    for (index, piece) in doc.as_bytes().chunks(16).enumerate() {
        collector.push_delta(std::str::from_utf8(piece).expect("ascii"));
        // A frame every 16 deltas: frames are paced by the display, not by the network.
        if index % 16 == 15 {
            let _ = collector.advance();
            advances += 1;
        }
    }
    let _ = collector.finalize_and_drain();
    assert!(advances > 500);
    let factor = collector.parsed_bytes() as f64 / doc.len() as f64;
    assert!(factor <= 4.0, "parsed {factor:.2}x the document");
}

#[test]
fn work_per_delta_does_not_grow_with_the_length_of_the_message() {
    // Advance after every 16-byte delta, the worst case, for a message and one twice as long.
    // Linear total work means the ratio stays near 2; quadratic would give 4.
    fn parsed(count: usize) -> usize {
        let doc = paragraphs(count);
        let mut collector = MarkdownStreamCollector::new(Some(80));
        for piece in doc.as_bytes().chunks(16) {
            collector.push_delta(std::str::from_utf8(piece).expect("ascii"));
            let _ = collector.advance();
        }
        collector.parsed_bytes()
    }
    let short = parsed(400);
    let long = parsed(800);
    let ratio = long as f64 / short as f64;
    assert!(
        ratio < 2.2,
        "doubling the message multiplied the work by {ratio:.2}"
    );
}

#[test]
fn an_open_table_is_withheld_and_shown_live() {
    let mut collector = MarkdownStreamCollector::new(None);
    collector.push_delta("intro\n\n| A | B |\n| --- | --- |\n| 1 | 2 |\n");
    let committed = collector.advance();
    assert_eq!(plain(&committed), vec!["intro".to_string()]);
    assert!(!collector.live_tail().is_empty());
}

#[test]
fn a_setext_heading_streams_as_the_paragraph_it_looked_like_when_committed() {
    // A line is committed when its newline arrives. A setext underline on the next line would
    // turn it into a heading, but the committed line cannot be taken back. The old newline-gated
    // stream behaved the same way; the consolidated transcript cell re-renders from the source.
    let streamed = streamed(&["Setext title\n", "============\n"], 1, None);
    assert_eq!(streamed, vec!["Setext title".to_string()]);
}

fn rust_fence(lines: usize) -> (String, usize) {
    let body: String = (0..lines)
        .map(|index| match index % 4 {
            0 => format!("fn item_{index}() -> u32 {{\n"),
            1 => format!("    let s = \"line {index}\"; // note\n"),
            2 => "\n".to_string(),
            _ => "}\n".to_string(),
        })
        .collect();
    let len = body.len();
    (
        format!("Some code:\n\n```rust\n{body}```\n\nAfter the code.\n"),
        len,
    )
}

#[test]
fn a_long_fence_is_highlighted_once_per_line_and_never_reparsed() {
    // A 5,000-line block streamed three bytes at a time with a frame after every delta: the
    // worst case. Each line is highlighted once and the markdown parser never sees the block.
    let (doc, body_len) = rust_fence(5000);
    let mut collector = MarkdownStreamCollector::new(Some(80));
    let mut out = Vec::new();
    for piece in doc.as_bytes().chunks(3) {
        collector.push_delta(std::str::from_utf8(piece).expect("ascii"));
        out.extend(collector.advance());
    }
    let parsed_while_streaming = collector.parsed_bytes();
    out.extend(collector.finalize_and_drain().0);
    let blank_lines = 5000 / 4;
    assert_eq!(collector.highlighted_bytes(), body_len - blank_lines);
    assert!(
        parsed_while_streaming < 64 * 1024,
        "the parser saw {parsed_while_streaming} bytes for a {}-byte message",
        doc.len()
    );
    assert_eq!(styled(&out), styled(&one_shot_lines(&doc, Some(80))));
}

#[test]
fn fence_lines_are_committed_as_they_complete_and_the_typed_line_is_live() {
    let mut collector = MarkdownStreamCollector::new(None);
    collector.push_delta("```rust\nlet a = 1;\nlet b");
    assert_eq!(plain(&collector.advance()), vec!["let a = 1;".to_string()]);
    assert_eq!(plain(collector.live_tail()), vec!["let b".to_string()]);
    // Blank lines are withheld until a line with content shows they are not the block's end.
    collector.push_delta(" = 2;\n\n");
    assert_eq!(plain(&collector.advance()), vec!["let b = 2;".to_string()]);
    collector.push_delta("c\n```\nafter\n");
    assert_eq!(
        plain(&collector.advance()),
        vec![
            "".to_string(),
            "c".to_string(),
            "".to_string(),
            "after".to_string()
        ]
    );
}

#[test]
fn a_table_inside_a_fence_stays_code() {
    let doc = "```\n| A | B |\n| --- | --- |\n| 1 | 2 |\n```\n";
    assert_eq!(
        one_shot(doc, None),
        vec![
            "| A | B |".to_string(),
            "| --- | --- |".to_string(),
            "| 1 | 2 |".to_string()
        ]
    );
}

/// A paragraph line of `words` tokens with `markup` spliced in after the given token indices.
fn long_line(words: usize, markup: &[(usize, &str)]) -> String {
    let mut line = String::new();
    for index in 0..words {
        if let Some((_, text)) = markup.iter().find(|(at, _)| *at == index) {
            line.push_str(text);
        }
        line.push_str(&format!("w{index:05}"));
        // Words of several lengths, so the rows break in different places.
        line.push_str(match index % 5 {
            0 => " and ",
            1 => ", ",
            2 => " is longer than most; ",
            _ => " ",
        });
    }
    line
}

#[test]
fn a_long_paragraph_line_commits_its_final_rows_while_it_is_typed() {
    let width = 40;
    let line = long_line(2_000, &[]);
    let mut collector = MarkdownStreamCollector::new(Some(width));
    let mut committed = Vec::new();
    let mut live_rows = Vec::new();
    for piece in line.as_bytes().chunks(32) {
        collector.push_delta(std::str::from_utf8(piece).expect("ascii"));
        committed.extend(collector.advance());
        live_rows.push(collector.live_tail().len());
    }
    let committed_while_typing = committed.len();
    committed.extend(collector.finalize_and_drain().0);

    assert_eq!(
        styled(&committed),
        styled(&one_shot_lines(&line, Some(width)))
    );
    // Nearly every row reached the scrollback before the line ended, and only the last few
    // rows were ever live once the line was long.
    assert!(
        committed_while_typing + 3 >= committed.len(),
        "{committed_while_typing} of {} rows committed while typing",
        committed.len()
    );
    assert!(
        live_rows.iter().rev().take(100).all(|rows| *rows <= 3),
        "{:?}",
        &live_rows[live_rows.len() - 100..]
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn any_split_of_a_long_line_with_markup_equals_the_one_shot_render(
        words in 200usize..900,
        markup in prop::collection::vec(
            (0usize..900, prop_oneof![
                Just("**"), Just("*"), Just("`"), Just("_"), Just("["), Just("]("), Just(")"),
                Just("<"), Just("~~"), Just("\\"), Just("&amp;"), Just("https://x.io/a-b "),
                Just("www.example.com "), Just("well-known "), Just("a/b "), Just("why? "),
                Just("averyveryveryveryveryveryverylongword "), Just("é "),
            ]),
            0..6,
        ),
        cuts in prop::collection::vec(0usize..40_000, 0..60),
        width in prop_oneof![Just(24usize), Just(37usize), Just(80usize)],
        trailing in prop_oneof![Just(""), Just("\n"), Just("\nnext line\n")],
    ) {
        let doc = format!("{}{trailing}", long_line(words, &markup));
        let pieces = split_at_chars(&doc, &cuts);
        let refs: Vec<&str> = pieces.iter().map(String::as_str).collect();
        prop_assert_eq!(
            styled(&streamed_lines(&refs, 1, Some(width))),
            styled(&one_shot_lines(&doc, Some(width)))
        );
    }
}

/// A `)` after a space keeps the word before it from ending a row until a URL later on the
/// line allows a break there; a row is final only once what follows its next word is plain too.
#[test]
fn a_row_is_not_final_while_a_closing_paren_follows_the_next_word() {
    let markup = [(243usize, "https://x.io/a-b "), (155, ")")];
    let doc = long_line(500, &markup);
    let pieces = split_at_chars(&doc, &[38616]);
    let refs: Vec<&str> = pieces.iter().map(String::as_str).collect();
    assert_eq!(
        styled(&streamed_lines(&refs, 1, Some(24))),
        styled(&one_shot_lines(&doc, Some(24)))
    );
}

#[test]
fn a_fence_in_a_nested_list_item_followed_by_indented_code_is_not_doubled() {
    let source = "- bullet one\n- bullet two\n  - nested bullet\n- bullet three\n  ```js\n  let x = 1;\n    nested\n  ```\n    indented code\n\n    still code\n";
    let whole = one_shot(source, None);
    let lines: Vec<&str> = source.split_inclusive('\n').collect();
    assert_eq!(streamed(&lines, 1, None), whole);
    assert_eq!(streamed(&[source], 1, None), whole);
}
