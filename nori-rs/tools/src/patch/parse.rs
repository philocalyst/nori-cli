//! Parsing the patch text into hunks.

use super::PatchError;

const BEGIN: &str = "*** Begin Patch";
const END: &str = "*** End Patch";
const ADD: &str = "*** Add File: ";
const DELETE: &str = "*** Delete File: ";
const UPDATE: &str = "*** Update File: ";
const MOVE: &str = "*** Move to: ";
const EOF_MARKER: &str = "*** End of File";

/// One change inside an updated file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Chunk {
    /// `@@ ...` lines that locate the change, outermost first.
    pub contexts: Vec<String>,
    pub old_lines: Vec<String>,
    pub new_lines: Vec<String>,
    /// The change is anchored at the end of the file.
    pub at_end_of_file: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hunk {
    Add {
        path: String,
        contents: String,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        move_to: Option<String>,
        chunks: Vec<Chunk>,
    },
}

fn line_no(index: usize) -> i64 {
    i64::try_from(index + 1).unwrap_or(i64::MAX)
}

pub fn parse_patch(patch: &str) -> Result<Vec<Hunk>, PatchError> {
    let lines: Vec<&str> = patch.lines().collect();
    let begin = lines
        .iter()
        .position(|line| line.trim() == BEGIN)
        .ok_or(PatchError::MissingEnvelope)?;
    let end = lines
        .iter()
        .rposition(|line| line.trim() == END)
        .filter(|end| *end > begin)
        .ok_or(PatchError::MissingEnvelope)?;

    let mut hunks = Vec::new();
    let mut i = begin + 1;
    while i < end {
        let line = lines[i];
        if line.trim().is_empty() {
            i += 1;
        } else if let Some(path) = line.strip_prefix(ADD) {
            let mut contents = String::new();
            i += 1;
            while i < end && !lines[i].starts_with("*** ") {
                let Some(added) = lines[i].strip_prefix('+') else {
                    return Err(PatchError::BadLine {
                        line_number: line_no(i),
                        path: path.trim().to_owned(),
                        line: lines[i].to_owned(),
                    });
                };
                contents.push_str(added);
                contents.push('\n');
                i += 1;
            }
            hunks.push(Hunk::Add {
                path: path.trim().to_owned(),
                contents,
            });
        } else if let Some(path) = line.strip_prefix(DELETE) {
            hunks.push(Hunk::Delete {
                path: path.trim().to_owned(),
            });
            i += 1;
        } else if let Some(path) = line.strip_prefix(UPDATE) {
            let path = path.trim().to_owned();
            i += 1;
            let move_to = lines
                .get(i)
                .filter(|_| i < end)
                .and_then(|line| line.strip_prefix(MOVE))
                .map(|target| target.trim().to_owned());
            if move_to.is_some() {
                i += 1;
            }
            let (chunks, next) = parse_chunks(&lines, i, end, &path)?;
            i = next;
            if chunks.is_empty() && move_to.is_none() {
                return Err(PatchError::EmptyUpdate { path });
            }
            hunks.push(Hunk::Update {
                path,
                move_to,
                chunks,
            });
        } else {
            return Err(PatchError::BadHeader {
                line_number: line_no(i),
                line: line.to_owned(),
            });
        }
    }
    if hunks.is_empty() {
        return Err(PatchError::Empty);
    }
    Ok(hunks)
}

fn parse_chunks(
    lines: &[&str],
    mut i: usize,
    end: usize,
    path: &str,
) -> Result<(Vec<Chunk>, usize), PatchError> {
    let mut chunks = Vec::new();
    let mut current = Chunk::default();
    let mut has_body = false;
    let flush = |current: &mut Chunk, has_body: &mut bool, chunks: &mut Vec<Chunk>| {
        if *has_body {
            chunks.push(std::mem::take(current));
        }
        *has_body = false;
    };
    while i < end {
        let line = lines[i];
        if line == EOF_MARKER {
            current.at_end_of_file = true;
            i += 1;
        } else if line.starts_with("*** ") {
            break;
        } else if let Some(context) = line.strip_prefix("@@") {
            if has_body {
                flush(&mut current, &mut has_body, &mut chunks);
            }
            let context = context.trim();
            if !context.is_empty() {
                current.contexts.push(context.to_owned());
            }
            i += 1;
        } else {
            match line.chars().next() {
                Some('+') => current.new_lines.push(line[1..].to_owned()),
                Some('-') => current.old_lines.push(line[1..].to_owned()),
                Some(' ') => {
                    current.old_lines.push(line[1..].to_owned());
                    current.new_lines.push(line[1..].to_owned());
                }
                // A blank line stands for a blank context line.
                None => {
                    current.old_lines.push(String::new());
                    current.new_lines.push(String::new());
                }
                Some(_) => {
                    return Err(PatchError::BadLine {
                        line_number: line_no(i),
                        path: path.to_owned(),
                        line: line.to_owned(),
                    });
                }
            }
            has_body = true;
            i += 1;
        }
    }
    flush(&mut current, &mut has_body, &mut chunks);
    Ok((chunks, i))
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn parses_all_three_operations_and_a_move() {
        let patch = "*** Begin Patch\n*** Add File: a.txt\n+one\n+two\n*** Delete File: b.txt\n\
            *** Update File: c.rs\n*** Move to: d.rs\n@@ fn main()\n keep\n-old\n+new\n*** End Patch\n";
        let hunks = parse_patch(patch).expect("parse");
        assert_eq!(
            hunks,
            vec![
                Hunk::Add {
                    path: "a.txt".into(),
                    contents: "one\ntwo\n".into()
                },
                Hunk::Delete {
                    path: "b.txt".into()
                },
                Hunk::Update {
                    path: "c.rs".into(),
                    move_to: Some("d.rs".into()),
                    chunks: vec![Chunk {
                        contexts: vec!["fn main()".into()],
                        old_lines: vec!["keep".into(), "old".into()],
                        new_lines: vec!["keep".into(), "new".into()],
                        at_end_of_file: false,
                    }],
                },
            ]
        );
    }

    #[test]
    fn rejects_a_missing_envelope_and_bad_lines() {
        assert!(matches!(
            parse_patch("*** Add File: a\n+x\n"),
            Err(PatchError::MissingEnvelope)
        ));
        assert!(matches!(
            parse_patch("*** Begin Patch\n*** Add File: a\nx\n*** End Patch"),
            Err(PatchError::BadLine { .. })
        ));
    }
}
