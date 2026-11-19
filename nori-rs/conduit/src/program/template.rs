//! `{{name}}` templates in step text.
//!
//! A reference names a binding made by an earlier step (`bind`), a fan-out's `each`, or the
//! guard names `output` and `feedback`. `\{{` writes a literal `{{`.

/// One piece of a parsed template.
#[derive(Debug, PartialEq, Eq)]
pub enum Piece<'a> {
    Text(String),
    Name(&'a str),
}

/// Split `text` into literal text and references. Fails on an unclosed or empty `{{`.
pub fn parse(text: &str) -> Result<Vec<Piece<'_>>, String> {
    let mut pieces = Vec::new();
    let mut literal = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("{{") {
        if rest[..at].ends_with('\\') {
            literal.push_str(&rest[..at - 1]);
            literal.push_str("{{");
            rest = &rest[at + 2..];
            continue;
        }
        literal.push_str(&rest[..at]);
        let after = &rest[at + 2..];
        let Some(end) = after.find("}}") else {
            return Err(format!(
                "`{{{{` opens a reference that is never closed in {text:?}; write `\\{{{{` for a literal"
            ));
        };
        let name = after[..end].trim();
        if name.is_empty()
            || !name
                .chars()
                .all(|ch| ch.is_alphanumeric() || matches!(ch, '_' | '-' | '.'))
        {
            return Err(format!(
                "`{{{{{}}}}}` is not a binding name (letters, digits, `_`, `-`, `.`)",
                &after[..end]
            ));
        }
        if !literal.is_empty() {
            pieces.push(Piece::Text(std::mem::take(&mut literal)));
        }
        pieces.push(Piece::Name(name));
        rest = &after[end + 2..];
    }
    literal.push_str(rest);
    if !literal.is_empty() {
        pieces.push(Piece::Text(literal));
    }
    Ok(pieces)
}

/// Fill a template; `lookup` returns the text of a binding, or `None` when it is not bound,
/// in which case the missing name is returned.
pub fn render(text: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    for piece in parse(text)? {
        match piece {
            Piece::Text(literal) => out.push_str(&literal),
            Piece::Name(name) => out.push_str(&lookup(name).ok_or_else(|| name.to_owned())?),
        }
    }
    Ok(out)
}
