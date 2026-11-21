//! Engine tests: every tier, line endings, ambiguity, misses and multi_edit rollback.

use pretty_assertions::assert_eq;

use super::*;

fn edit(old: &str, new: &str) -> Edit {
    Edit {
        old_string: old.to_owned(),
        new_string: new.to_owned(),
        replace_all: false,
    }
}

fn run(content: &str, old: &str, new: &str) -> (String, EditReport) {
    let mut doc = Document::parse(content);
    let report = doc.apply(&edit(old, new)).expect("edit applies");
    (doc.render(), report)
}

fn run_err(content: &str, old: &str, new: &str) -> EditError {
    let mut doc = Document::parse(content);
    doc.apply(&edit(old, new)).expect_err("edit fails")
}

#[test]
fn exact_match_replaces_one_place() {
    let (out, report) = run("let a = 1;\nlet b = 2;\n", "let b = 2;", "let b = 3;");
    assert_eq!(out, "let a = 1;\nlet b = 3;\n");
    assert_eq!(report.tier, MatchTier::Exact);
    assert_eq!(report.lines, vec![2]);
    assert!(report.notes.is_empty());
}

#[test]
fn exact_match_can_be_part_of_a_line_and_delete_text() {
    let (out, _) = run("foo.bar(1)\n", "bar(", "baz(");
    assert_eq!(out, "foo.baz(1)\n");
    let (out, _) = run("a\nb\nc\n", "b\n", "");
    assert_eq!(out, "a\nc\n");
}

#[test]
fn crlf_file_matches_an_lf_old_string_and_keeps_its_endings() {
    let content = "fn a() {\r\n    1\r\n}\r\nfn b() {}\r\n";
    let (out, report) = run(content, "fn a() {\n    1\n}", "fn a() {\n    2\n    3\n}");
    assert_eq!(out, "fn a() {\r\n    2\r\n    3\r\n}\r\nfn b() {}\r\n");
    assert_eq!(report.tier, MatchTier::LineEndings);
}

#[test]
fn crlf_in_old_string_matches_an_lf_file() {
    let (out, _) = run("a\nb\n", "a\r\nb", "x\r\ny");
    assert_eq!(out, "x\ny\n");
}

#[test]
fn mixed_endings_keep_untouched_lines_as_they_were() {
    let (out, _) = run("a\r\nb\nc\r\n", "b", "B");
    assert_eq!(out, "a\r\nB\nc\r\n");
}

#[test]
fn bom_is_preserved() {
    let (out, _) = run("\u{FEFF}hello\n", "hello", "bye");
    assert_eq!(out, "\u{FEFF}bye\n");
}

#[test]
fn curly_quotes_dashes_and_nbsp_in_the_file_match_plain_text() {
    let content = "say \u{201C}hi\u{201D} \u{2014} it\u{2019}s\u{00A0}fine\n";
    let (out, report) = run(content, "say \"hi\" - it's fine", "say \"bye\"");
    assert_eq!(out, "say \"bye\"\n");
    assert_eq!(report.tier, MatchTier::Unicode);
}

#[test]
fn plain_quotes_in_the_file_match_curly_quotes_from_the_model() {
    let (out, report) = run("print('x')\n", "print(\u{2018}x\u{2019})", "print('y')");
    assert_eq!(out, "print('y')\n");
    assert_eq!(report.tier, MatchTier::Unicode);
}

#[test]
fn decomposed_accents_match_composed_ones() {
    let (out, report) = run("cafe\u{0301} au lait\n", "caf\u{00E9}", "tea");
    assert_eq!(out, "tea au lait\n");
    assert_eq!(report.tier, MatchTier::Unicode);
}

#[test]
fn trailing_whitespace_differences_are_ignored() {
    let content = "let a = 1;  \nlet b = 2;\t\nlet c = 3;\n";
    let (out, report) = run(
        content,
        "let a = 1;\nlet b = 2;",
        "let a = 10;\nlet b = 20;",
    );
    assert_eq!(out, "let a = 10;\nlet b = 20;\nlet c = 3;\n");
    assert_eq!(report.tier, MatchTier::TrailingWhitespace);
}

#[test]
fn indentation_is_ignored_and_new_text_is_reindented_by_adding_spaces() {
    let content = "impl A {\n    fn f() {\n        work();\n    }\n}\n";
    let (out, report) = run(
        content,
        "fn f() {\n    work();\n}",
        "fn f() {\n    work();\n    more();\n}",
    );
    assert_eq!(
        out,
        "impl A {\n    fn f() {\n        work();\n        more();\n    }\n}\n"
    );
    assert_eq!(report.tier, MatchTier::Indentation);
    assert_eq!(
        report.notes,
        vec!["matched ignoring indentation; re-indented (+4 spaces)".to_owned()]
    );
}

#[test]
fn indentation_tier_removes_extra_indentation_the_model_added() {
    let content = "a\nb\n";
    let (out, _) = run(content, "    a\n    b", "    a\n    c");
    assert_eq!(out, "a\nc\n");
}

#[test]
fn indentation_tier_converts_spaces_to_the_files_tabs() {
    let content = "fn f() {\n\tlet x = 1;\n\tif x {\n\t\tg();\n\t}\n}\n";
    let (out, report) = run(
        content,
        "    let x = 1;\n    if x {\n        g();\n    }",
        "    let x = 2;\n    if x {\n        h();\n    }",
    );
    assert_eq!(out, "fn f() {\n\tlet x = 2;\n\tif x {\n\t\th();\n\t}\n}\n");
    assert_eq!(report.tier, MatchTier::Indentation);
    assert!(
        report.notes[0].contains("converted spaces to tabs"),
        "{:?}",
        report.notes
    );
}

#[test]
fn indentation_tier_keeps_crlf() {
    let content = "  a\r\n  b\r\n";
    let (out, _) = run(content, "a\nb", "a\nB\nc");
    assert_eq!(out, "  a\r\n  B\r\n  c\r\n");
}

#[test]
fn a_blank_only_old_string_never_matches_by_indentation() {
    let err = run_err("a\n   \nb\n", "\n\n", "x");
    assert!(matches!(err, EditError::NoMatch(_)));
}

#[test]
fn multiple_exact_matches_are_refused_with_every_line_number() {
    let err = run_err("x = 1\ny = 2\nx = 1\n", "x = 1", "x = 9");
    let EditError::Ambiguous { tier, sites } = &err else {
        panic!("expected ambiguity, got {err:?}");
    };
    assert_eq!(*tier, MatchTier::Exact);
    assert_eq!(
        sites.iter().map(|site| site.line).collect::<Vec<_>>(),
        vec![1, 3]
    );
    let text = err.to_string();
    assert!(text.contains("line 1") && text.contains("line 3"), "{text}");
    assert!(text.contains("replace_all"), "{text}");
}

#[test]
fn ambiguity_in_a_looser_tier_is_also_refused() {
    let err = run_err("  a  \nb\n  a  \n", "a", "z");
    // "a" occurs exactly in both lines; this is an exact-tier ambiguity.
    assert!(matches!(err, EditError::Ambiguous { .. }));
    let err = run_err("\ta\n\ta\n", "    a", "z");
    let EditError::Ambiguous { tier, .. } = err else {
        panic!("expected ambiguity");
    };
    assert_eq!(tier, MatchTier::Indentation);
}

#[test]
fn replace_all_replaces_every_occurrence_in_one_pass() {
    let mut doc = Document::parse("a b a\r\na\r\n");
    let report = doc
        .apply(&Edit {
            old_string: "a".into(),
            new_string: "xy".into(),
            replace_all: true,
        })
        .expect("apply");
    assert_eq!(doc.render(), "xy b xy\r\nxy\r\n");
    assert_eq!(report.replacements, 3);
    assert_eq!(report.lines, vec![1, 1, 2]);
}

#[test]
fn replace_all_reindents_each_match_on_its_own() {
    let content = "\tf()\nx\n\t\tf()\n";
    let mut doc = Document::parse(content);
    doc.apply(&Edit {
        old_string: "    f()".into(),
        new_string: "    g()".into(),
        replace_all: true,
    })
    .expect("apply");
    assert_eq!(doc.render(), "\tg()\nx\n\t\tg()\n");
}

#[test]
fn a_miss_shows_the_closest_lines_and_what_differs() {
    let content =
        "fn alpha() {}\nfn compute(value: i32) -> i32 {\n    value * 2\n}\nfn omega() {}\n";
    let err = run_err(
        content,
        "fn compute(value: i64) -> i32 {\n    value * 2\n}",
        "x",
    );
    let text = err.to_string();
    assert!(text.contains("not found"), "{text}");
    assert!(text.contains("2→fn compute(value: i32)"), "{text}");
    assert!(
        text.contains("you wrote `fn compute(value: i64) -> i32 {`"),
        "{text}"
    );
    assert!(text.contains("similar"), "{text}");
}

#[test]
fn a_miss_that_is_only_whitespace_in_the_middle_of_a_line_explains_it() {
    let err = run_err("let a  =  1;\n", "let a = 1;", "x");
    let text = err.to_string();
    assert!(text.contains("let a  =  1;"), "{text}");
}

#[test]
fn a_miss_notices_when_the_edit_was_already_applied() {
    let err = run_err("let value = 2;\n", "let value = 1;", "let value = 2;");
    assert!(err.to_string().contains("already"), "{err}");
}

#[test]
fn empty_and_identical_old_strings_are_errors() {
    assert!(matches!(
        run_err("abc\n", "", "x"),
        EditError::EmptyOldString
    ));
    assert!(matches!(
        run_err("abc\n", "abc", "abc"),
        EditError::Identical
    ));
}

#[test]
fn empty_old_string_fills_an_empty_file() {
    let (out, _) = run("", "", "hello\n");
    assert_eq!(out, "hello\n");
}

#[test]
fn line_number_prefixes_from_read_are_stripped() {
    let content = "fn a() {\n    1\n}\n";
    let (out, report) = run(
        content,
        "     1→fn a() {\n     2→    1\n     3→}",
        "     1→fn a() {\n     2→    2\n     3→}",
    );
    assert_eq!(out, "fn a() {\n    2\n}\n");
    assert!(
        report.notes[0].contains("line-number prefixes"),
        "{:?}",
        report.notes
    );
}

#[test]
fn a_stray_arrow_in_code_is_not_a_line_number_prefix() {
    assert_eq!(strip_line_numbers("a → b"), None);
    assert_eq!(strip_line_numbers("x\n12→y"), None);
    assert_eq!(
        strip_line_numbers("  7→y\n\n  8→z"),
        Some("y\n\nz".to_owned())
    );
}

#[test]
fn multi_edit_applies_in_order_and_later_edits_see_earlier_results() {
    let mut doc = Document::parse("one\ntwo\n");
    let reports = doc
        .apply_all(&[edit("one", "uno"), edit("uno\ntwo", "UNO\nDOS")])
        .expect("apply_all");
    assert_eq!(reports.len(), 2);
    assert_eq!(doc.render(), "UNO\nDOS\n");
}

#[test]
fn multi_edit_is_all_or_nothing() {
    let mut doc = Document::parse("one\ntwo\n");
    let err = doc
        .apply_all(&[edit("one", "uno"), edit("missing", "x")])
        .expect_err("second edit fails");
    assert_eq!(doc.render(), "one\ntwo\n");
    let text = err.to_string();
    assert!(
        text.contains("Edit 2 of 2") && text.contains("nothing was written"),
        "{text}"
    );
}

#[test]
fn multi_edit_without_edits_is_an_error() {
    assert!(matches!(
        Document::parse("a").apply_all(&[]),
        Err(EditError::NoEdits)
    ));
}

#[test]
fn replacing_text_with_what_the_fuzzy_tier_normalises_to_is_a_no_op() {
    let mut doc = Document::parse("say \u{201C}hi\u{201D}\n");
    let before = doc.render();
    doc.apply(&edit("say \"hi\"", "say \u{201C}hi\u{201D}"))
        .expect("apply");
    assert_eq!(doc.render(), before);
}

#[test]
fn unified_diff_and_counts_describe_the_change() {
    assert_eq!(count_changes("a\nb\n", "a\nc\nd\n"), (2, 1));
    assert!(unified_diff("f", "a\n", "b\n").contains("-a\n+b"));
}
