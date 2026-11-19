//! Java quoted regex literals remain literal through Pkl's anchored operations.

use super::*;
use pretty_assertions::assert_eq;

#[test]
fn unterminated_quotes_end_before_generated_anchors() {
    assert_eq!(
        eval(
            r###"
result = List("x)".matches(Regex(#"x\Q)"#)), "x)z".startsWith(Regex(#"x\Q)"#)),
  "zx)".endsWith(Regex(#"x\Q)"#)), "x".matches(Regex(#"x\Q)"#)), "x)z".endsWith(Regex(#"x\Q)"#)))
"###
        ),
        serde_json::json!({"result": [true, true, true, false, false]})
    );
}

#[test]
fn extended_mode_preserves_quoted_spaces_and_comment_markers() {
    assert_eq!(
        eval(
            r###"
result = List("x # text".matches(Regex(#"(?x)x\Q # text\E"#)),
  "x # text".matches(Regex(#"(?x)x\Q # text"#)),
  "x # text!".startsWith(Regex(#"(?x)x\Q # text"#)),
  "!x # text".endsWith(Regex(#"(?x)x\Q # text"#)),
  "xtext".matches(Regex(#"(?x)x\Q # text\E"#)))
"###
        ),
        serde_json::json!({"result": [true, true, true, true, false]})
    );
}

#[test]
fn quoted_newlines_remain_literal_in_extended_mode() {
    assert_eq!(
        eval(
            r###"
local pattern = Regex(#"""
(?x)\Qa
b\E
"""#)
result = List("a\nb".matches(pattern), "ab".matches(pattern), "a\nb!".startsWith(pattern), "!a\nb".endsWith(pattern))
"###
        ),
        serde_json::json!({"result": [true, false, true, true]})
    );
}

#[test]
fn quote_markers_inside_extended_comments_are_comment_text() {
    assert_eq!(
        eval(
            r###"
local continued = Regex(#"""
(?x)x # \Qignored
y
"""#)
local trailing = Regex(#"(?x)x # \Qignored"#)
result = List("xy".matches(continued), "xy!".startsWith(continued), "!xy".endsWith(continued),
  "x".matches(trailing), "x!".startsWith(trailing), "!x".endsWith(trailing), "xignored".matches(trailing))
"###
        ),
        serde_json::json!({"result": [true, true, true, true, true, true, false]})
    );
}

#[test]
fn scoped_extended_flags_restore_literal_context() {
    assert_eq!(
        eval(
            r###"
result = List("a b".matches(Regex(#"(?x:a\Q \Eb)"#)),
  "a b".matches(Regex(#"(?x)a(?-x: b)"#)),
  "x#y".matches(Regex(#"(?x:x)#\Qy"#)),
  "ab".matches(Regex(#"(?x:a\Q \Eb)"#)))
"###
        ),
        serde_json::json!({"result": [true, true, true, false]})
    );
}

#[test]
fn escaped_backslashes_do_not_open_quotes() {
    assert_eq!(
        eval(
            r###"
result = List(#"\Q"#.matches(Regex(#"\\Q"#)), #"\E"#.matches(Regex(#"\\E"#)),
  #"\Qx\E"#.matches(Regex(#"\Q\Qx\E\\E"#)), "Q".matches(Regex(#"\\Q"#)))
"###
        ),
        serde_json::json!({"result": [true, true, true, false]})
    );
}

#[test]
fn quoted_character_class_members_stay_literal() {
    assert_eq!(
        eval(
            r###"
result = List("]# ".matches(Regex(#"(?x)[\Q]# \E]+"#)),
  "]# ".matches(Regex(#"(?x)[]\Q# \E]+"#)),
  "z".matches(Regex(#"(?x)[\Q]# \E]+"#)))
"###
        ),
        serde_json::json!({"result": [true, true, false]})
    );
}

#[test]
fn regex_identity_and_pattern_text_use_the_original_source() {
    assert_eq!(
        eval(
            r###"
pattern = Regex(#"(?x)\Q # é\E"#).pattern
sameSource = Regex(#"\Qx\E"#) == Regex(#"\Qx\E"#)
differentSource = Regex(#"\Qx\E"#) == Regex("x")
"###
        ),
        serde_json::json!({"pattern": "(?x)\\Q # é\\E", "sameSource": true, "differentSource": false})
    );
}

#[test]
fn matching_replacement_and_splitting_share_quote_semantics() {
    assert_eq!(
        eval(
            r###"
local pattern = Regex(#"(?x)\Q # \E"#)
matches = pattern.findMatchesIn("a # b # c").map((m) -> m.value)
split = "a # b # c".split(pattern)
replacement = "a # b # c".replaceAll(pattern, "-")
"###
        ),
        serde_json::json!({"matches": [" # ", " # "], "split": ["a", "b", "c"], "replacement": "a-b-c"})
    );
}

#[test]
fn anchors_preserve_inline_flag_positions_and_overlapping_suffixes() {
    assert_eq!(
        eval(
            r###"
result = List("Ab".endsWith(Regex("a(?i)b")), "x\n".endsWith(Regex("x.(?s)")),
  "a b".endsWith(Regex("a b(?x)")), "aaa".endsWith(Regex("aa")),
  "éÉ".matches(Regex(#"(?i)\Qéé\E"#)))
"###
        ),
        serde_json::json!({"result": [false, false, true, true, true]})
    );
}

#[test]
fn extended_mode_ignores_ascii_whitespace_including_inside_classes() {
    assert_eq!(
        eval(
            r###"
result = List(" ".matches(Regex("(?x)[a b]")), "a".matches(Regex("(?x)[a b]")),
  "a\u{b}b".matches(Regex("(?x)a\u{b}b")), "ab".matches(Regex("(?x)a\u{b}b")),
  "a\u{c}b".matches(Regex("(?x)a\u{c}b")), "ab".matches(Regex("(?x)a\u{c}b")),
  "a\u{2003}b".matches(Regex("(?x)a\u{2003}b")))
"###
        ),
        serde_json::json!({"result": [false, true, false, true, false, true, true]})
    );
}
