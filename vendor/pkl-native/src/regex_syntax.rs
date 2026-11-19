//! Java regex quoting and lexical boundaries shared by compilation and anchors.

use std::borrow::Cow;
use std::fmt::Write;

pub(crate) struct Pattern<'a> {
    pub(crate) text: Cow<'a, str>,
    pub(crate) trailing_comment: bool,
}

#[derive(Clone, Copy)]
enum ClassPosition {
    Start,
    Negated,
    Member,
}

/// Expand Java's `\Q...\E` into literal code points, including an unterminated
/// quotation. Hex escapes keep whitespace and `#` literal in extended mode.
/// Quotation markers in comments stay comment text.
pub(crate) fn normalize(pattern: &str) -> Pattern<'_> {
    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.char_indices().peekable();
    let mut extended = false;
    let mut group_modes = Vec::new();
    let mut classes = Vec::new();
    let mut trailing_comment = false;
    while let Some((offset, ch)) = chars.next() {
        match ch {
            '\\' => match chars.next() {
                Some((_, 'Q')) => {
                    while let Some((_, literal)) = chars.next() {
                        if literal == '\\' && chars.peek().is_some_and(|(_, ch)| *ch == 'E') {
                            chars.next();
                            break;
                        }
                        if let Some(position) = classes.last_mut() {
                            *position = ClassPosition::Member;
                        }
                        write!(&mut out, "\\x{{{:x}}}", literal as i64)
                            .expect("writing to String cannot fail");
                    }
                }
                Some((_, escaped)) => {
                    out.push('\\');
                    out.push(escaped);
                    if let Some(position) = classes.last_mut() {
                        *position = ClassPosition::Member;
                    }
                }
                None => out.push('\\'),
            },
            '#' if extended => {
                out.push(ch);
                trailing_comment = true;
                for (_, comment) in chars.by_ref() {
                    out.push(comment);
                    if comment == '\n' {
                        trailing_comment = false;
                        break;
                    }
                }
            }
            '[' => {
                if let Some(position) = classes.last_mut() {
                    *position = ClassPosition::Member;
                }
                classes.push(ClassPosition::Start);
                out.push(ch);
            }
            '^' if matches!(classes.last(), Some(ClassPosition::Start)) => {
                *classes.last_mut().expect("class exists") = ClassPosition::Negated;
                out.push(ch);
            }
            ']' if !classes.is_empty() => {
                if matches!(classes.last(), Some(ClassPosition::Member)) {
                    classes.pop();
                } else {
                    *classes.last_mut().expect("class exists") = ClassPosition::Member;
                }
                out.push(ch);
            }
            '(' if classes.is_empty() => {
                if let Some((end, scoped, mode)) = inline_extended_mode(pattern.as_bytes(), offset)
                {
                    if scoped {
                        group_modes.push(extended);
                    }
                    extended = mode;
                    out.push_str(&pattern[offset..=end]);
                    while chars.peek().is_some_and(|(offset, _)| *offset <= end) {
                        chars.next();
                    }
                } else {
                    group_modes.push(extended);
                    out.push(ch);
                }
            }
            ')' if classes.is_empty() => {
                if let Some(mode) = group_modes.pop() {
                    extended = mode;
                }
                out.push(ch);
            }
            _ if extended && matches!(ch, ' ' | '\t' | '\n' | '\r' | '\u{b}' | '\u{c}') => {}
            _ => {
                if let Some(position) = classes.last_mut() {
                    *position = ClassPosition::Member;
                }
                out.push(ch);
            }
        }
    }
    Pattern {
        text: if out == pattern {
            pattern.into()
        } else {
            out.into()
        },
        trailing_comment,
    }
}

/// An inline flag group that changes extended mode: delimiter, scopedness,
/// and the new mode. Other flags stay in the source for the regex engine.
pub(crate) fn inline_extended_mode(bytes: &[u8], start: usize) -> Option<(usize, bool, bool)> {
    if bytes.get(start + 1) != Some(&b'?') {
        return None;
    }
    let mut i = start + 2;
    let mut disabled = false;
    let mut mode = None;
    while let Some(&byte) = bytes.get(i) {
        match byte {
            b'-' => disabled = true,
            b'x' => mode = Some(!disabled),
            b'a'..=b'z' | b'A'..=b'Z' => {}
            b':' | b')' => return mode.map(|mode| (i, byte == b':', mode)),
            _ => return None,
        }
        i += 1;
    }
    None
}
