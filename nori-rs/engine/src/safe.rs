//! Shell commands the engine knows only read: the permission action [`READ_ONLY_SHELL`].
//!
//! A mode can allow these while other commands ask (Codex's `auto` and `read-only` presets, which
//! Codex backs with a sandbox the engine does not have). The list is Codex's known-safe one and
//! deliberately small: a command qualifies only when every part of it is a known reader with no
//! option that writes, runs another program or redirects, and nothing in it expands (`$`,
//! backquotes, redirections, subshells). `&&`, `||`, `;` and `|` may join such commands;
//! `bash -lc '<script>'` is judged by its script.

/// The permission action of a shell command that only reads. It is evaluated only when the
/// `shell` rules ask, and only rules naming this action exactly decide it.
pub const READ_ONLY_SHELL: &str = "read_only_shell";

/// Whether `command` is known to only read.
pub fn reads_only(command: &str) -> bool {
    match split(command) {
        Some(parts) if !parts.is_empty() => parts.iter().all(|part| {
            shlex::split(part).is_some_and(|words| !words.is_empty() && words_read_only(&words))
        }),
        Some(_) | None => false,
    }
}

/// The commands of `script` joined by `&&`, `||`, `;` or `|`, or `None` when anything in it
/// expands, redirects, backgrounds or opens a subshell.
fn split(script: &str) -> Option<Vec<String>> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = script.trim().chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some('\''), '\'') | (Some('"'), '"') => {
                quote = None;
                current.push(c);
            }
            (Some('"'), '$' | '`' | '\\') => return None,
            (Some(_), _) => current.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                current.push(c);
            }
            (None, ';') => parts.push(std::mem::take(&mut current)),
            (None, '|') => {
                if chars.peek() == Some(&'|') {
                    chars.next();
                }
                parts.push(std::mem::take(&mut current));
            }
            (None, '&') => {
                if chars.next() != Some('&') {
                    return None;
                }
                parts.push(std::mem::take(&mut current));
            }
            (None, '$' | '`' | '<' | '>' | '(' | ')' | '\\' | '\n' | '\r' | '{' | '}') => {
                return None;
            }
            (None, _) => current.push(c),
        }
    }
    if quote.is_some() {
        return None;
    }
    parts.push(current);
    let parts: Vec<String> = parts
        .into_iter()
        .map(|part| part.trim().to_owned())
        .collect();
    (!parts.iter().any(String::is_empty)).then_some(parts)
}

fn words_read_only(words: &[String]) -> bool {
    let Some((program, args)) = words.split_first() else {
        return false;
    };
    let name = std::path::Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program);
    let has = |flags: &[&str]| {
        args.iter().any(|arg| {
            flags
                .iter()
                .any(|flag| arg == flag || arg.starts_with(&format!("{flag}=")))
        })
    };
    match name {
        "cat" | "cd" | "echo" | "false" | "grep" | "head" | "ls" | "nl" | "pwd" | "tail"
        | "true" | "wc" | "which" | "whoami" | "uname" | "file" | "stat" | "tree" | "du" | "df"
        | "basename" | "dirname" | "realpath" | "readlink" | "cut" | "tr" | "printf" => true,
        "sort" => !has(&["-o", "--output"]) && !args.iter().any(|arg| arg.starts_with("-o")),
        "date" => !has(&["-s", "--set"]),
        "find" => !has(&[
            "-exec", "-execdir", "-ok", "-okdir", "-delete", "-fls", "-fprint", "-fprint0",
            "-fprintf",
        ]),
        "rg" => !has(&["--pre", "--hostname-bin", "--search-zip", "-z"]),
        "sed" => {
            matches!(
                args.iter().map(String::as_str).collect::<Vec<_>>().as_slice(),
                ["-n", range, ..] if sed_print_range(range)
            ) && args.len() <= 3
        }
        "git" => match args.split_first() {
            Some((sub, rest)) => {
                let writes = rest.iter().any(|arg| {
                    arg.starts_with("--output") || arg == "-o" || arg.starts_with("--ext-diff")
                });
                !writes
                    && match sub.as_str() {
                        "status" | "log" | "diff" | "show" | "rev-parse" | "ls-files" | "blame" => {
                            true
                        }
                        "branch" => rest.iter().all(|arg| {
                            matches!(
                                arg.as_str(),
                                "-a" | "-r"
                                    | "-v"
                                    | "-vv"
                                    | "--all"
                                    | "--list"
                                    | "--show-current"
                                    | "--remotes"
                                    | "--no-color"
                                    | "--color"
                            )
                        }),
                        _ => false,
                    }
            }
            None => false,
        },
        "bash" | "sh" | "zsh" => match args
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .as_slice()
        {
            ["-c" | "-lc", script] => reads_only(script),
            _ => false,
        },
        _ => false,
    }
}

/// `sed -n` with `N p` or `M,N p`: print lines, nothing else.
fn sed_print_range(range: &str) -> bool {
    let Some(numbers) = range.strip_suffix('p') else {
        return false;
    };
    let mut bounds = numbers.split(',');
    let numeric = |part: Option<&str>| {
        part.is_some_and(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
    };
    let first = bounds.next();
    match (first, bounds.next(), bounds.next()) {
        (Some(_), None, None) => numeric(first),
        (Some(_), Some(second), None) => numeric(first) && numeric(Some(second)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::reads_only;

    #[test]
    fn readers_pass_and_anything_that_writes_or_expands_asks() {
        for command in [
            "ls -la",
            "git status",
            "git log --oneline -5 && git diff HEAD~1",
            "rg -n 'a|b' src | head -20",
            "find . -name '*.rs'",
            "sed -n 10,20p src/lib.rs",
            "bash -lc 'cat README.md; pwd'",
            "cat \"a b.txt\"",
        ] {
            assert!(reads_only(command), "{command}");
        }
        for command in [
            "rm -rf target",
            "cat a > b",
            "ls; rm x",
            "echo $(whoami)",
            "echo \"$HOME\"",
            "find . -delete",
            "find . -exec rm {} ;",
            "rg --pre cat x",
            "git branch -D main",
            "git diff --output=patch",
            "git push",
            "sed -i s/a/b/ f",
            "sort -o out in",
            "sleep 1 &",
            "bash -lc 'rm -rf /'",
            "cat 'unterminated",
            "",
        ] {
            assert!(!reads_only(command), "{command}");
        }
    }
}
