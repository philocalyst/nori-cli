//! The POSIX `sh` scripts Nori sends to a machine.
//!
//! A script travels on ssh's stdin to `/bin/sh -s`, the one remote command every login shell
//! (bash, zsh, fish, nu, csh) runs the same way. The whole script is one `{ ... }` group, so the
//! shell has read all of it before anything runs; what is left on stdin afterwards is the
//! connection's lifeline: when Nori closes it (a timeout, a stop) or dies, a watchdog reading it
//! sees end of file and kills the command's whole process group. Nothing Nori starts on a machine
//! outlives the connection that started it.
//!
//! Progress comes back on stderr as lines starting with [`MARK`], which no program prints by
//! accident: `start cwd=DIR` once the command's directory is entered, `nodir DIR` when a directory
//! Nori was told to use does not exist, and `end status=N cwd=DIR` when it exits.

/// The start of every line the scripts report on: ASCII record separator, then `nori:`.
pub(crate) const MARK: &str = "\u{1e}nori:";

/// Facts about the machine, one `key=value` per line. `path` is the `PATH` of the user's login
/// shell, so commands find what an interactive terminal on that machine would.
pub(crate) const PROBE: &str = r#"{
printf 'nori-probe 1\n'
printf 'os=%s\n' "$(uname -s 2>/dev/null)"
printf 'arch=%s\n' "$(uname -m 2>/dev/null)"
printf 'release=%s\n' "$(uname -r 2>/dev/null)"
printf 'hostname=%s\n' "$(hostname 2>/dev/null || uname -n 2>/dev/null)"
printf 'home=%s\n' "$HOME"
printf 'shell=%s\n' "$SHELL"
printf 'bash=%s\n' "$(command -v bash 2>/dev/null)"
printf 'path=%s\n' "$("${SHELL:-/bin/sh}" -l -c /usr/bin/env </dev/null 2>/dev/null | sed -n 's/^PATH=//p' | head -n 1)"
exit 0
}
"#;

/// Where a command starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteDir {
    /// An absolute path, or one starting with `~/`.
    Path(String),
    /// The machine's home directory.
    Home,
}

impl RemoteDir {
    /// `~`, `~/x` and absolute paths; anything else is relative to the home directory.
    pub fn parse(dir: &str) -> Self {
        let dir = dir.trim();
        if dir.is_empty() || dir == "~" || dir == "~/" {
            Self::Home
        } else {
            Self::Path(dir.to_owned())
        }
    }

    /// The directory as a shell word, `~` expanded by the remote shell.
    pub(crate) fn word(&self) -> String {
        match self {
            Self::Home => "\"$HOME\"".to_owned(),
            Self::Path(path) => match path.strip_prefix("~/") {
                Some(rest) => format!("\"$HOME\"/{}", quote(rest)),
                None if path.starts_with('/') => quote(path),
                None => format!("\"$HOME\"/{}", quote(path)),
            },
        }
    }
}

/// Where to start: a directory that must exist, or the first of some candidates that does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartDir {
    /// The agent named it: if it is missing, the command does not run.
    Required(RemoteDir),
    /// Tried in order; the home directory when none exists.
    FirstOf(Vec<RemoteDir>),
}

/// A command to run, and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExecScript<'a> {
    pub(crate) command: &'a str,
    pub(crate) start: &'a StartDir,
    /// The login shell's `PATH`, when the machine was probed.
    pub(crate) path: Option<&'a str>,
}

impl ExecScript<'_> {
    /// The script. The command runs in bash when the machine has it (else `sh`), as
    /// `bash -c` with the command as an argument, so nothing in it can break the script around
    /// it. An `EXIT` trap writes the directory it ended in, which the next command starts from.
    pub(crate) fn render(&self) -> String {
        // fd 3 keeps the connection's stdin: a background job's own stdin is /dev/null.
        let mut script = String::from("{\nexec 3<&0\n");
        if let Some(path) = self.path.filter(|path| !path.is_empty()) {
            script.push_str(&format!("PATH={}; export PATH\n", quote(path)));
        }
        script.push_str("__nori_m=$(printf '\\036nori:')\n");
        script.push_str(&cd_lines(self.start));
        script.push_str(
            r#"__nori_state=$(mktemp "${TMPDIR:-/tmp}/nori-cwd.XXXXXX" 2>/dev/null || printf '%s' "${TMPDIR:-/tmp}/nori-cwd.$$")
__nori_sh=$(command -v bash 2>/dev/null || printf /bin/sh)
printf '%sstart cwd=%s\n' "$__nori_m" "$PWD" >&2
"#,
        );
        script.push_str(&format!("__nori_cmd={}\n", quote(self.command)));
        script.push_str(
            r#""$__nori_sh" -c 'trap "pwd >\"\$0\" 2>/dev/null" EXIT; eval "$1"' "$__nori_state" "$__nori_cmd" </dev/null 3<&- &
__nori_child=$!
( trap '' TERM; while IFS= read -r __nori_line; do :; done; kill -TERM 0 2>/dev/null; sleep 2; kill -KILL 0 2>/dev/null ) <&3 3<&- &
__nori_watch=$!
wait "$__nori_child"
__nori_status=$?
kill -KILL "$__nori_watch" 2>/dev/null
wait "$__nori_watch" 2>/dev/null
__nori_cwd=$(cat "$__nori_state" 2>/dev/null)
rm -f "$__nori_state"
printf '%send status=%s cwd=%s\n' "$__nori_m" "$__nori_status" "${__nori_cwd:-$PWD}" >&2
exit "$__nori_status"
}
"#,
        );
        script
    }
}

/// Lines that enter the start directory, defining `$__nori_m` first (a required directory that
/// is missing reports `nodir` and exits 2).
pub(crate) fn cd_lines(start: &StartDir) -> String {
    let mut script = String::new();
    match start {
        StartDir::Required(dir) => {
            let word = dir.word();
            script.push_str(&format!(
                "cd -- {word} 2>/dev/null || {{ printf '%snodir %s\\n' \"$__nori_m\" {word} >&2; exit 2; }}\n"
            ));
        }
        StartDir::FirstOf(dirs) => {
            let words: Vec<String> = dirs.iter().map(RemoteDir::word).collect();
            script.push_str("__nori_dir=\n");
            if !words.is_empty() {
                script.push_str(&format!(
                    "for __nori_try in {}; do if [ -d \"$__nori_try\" ]; then __nori_dir=$__nori_try; break; fi; done\n",
                    words.join(" ")
                ));
            }
            script.push_str(
                "cd -- \"${__nori_dir:-$HOME}\" 2>/dev/null || cd -- \"$HOME\" 2>/dev/null || cd /\n",
            );
        }
    }
    script
}

/// Write the base64 `body` to `dir/file_name` (decoded), atomically, and print
/// `MARK uploaded PATH`.
pub(crate) fn upload(dir: &RemoteDir, file_name: &str, body: &str) -> String {
    let dir = dir.word();
    let name = quote(file_name);
    let delimiter = "NORI_UPLOAD_7f3a9c";
    format!(
        r#"{{
__nori_m=$(printf '\036nori:')
mkdir -p {dir} && cd {dir} || exit 3
cat > {name}.b64 <<'{delimiter}'
{body}{delimiter}
if base64 -d < {name}.b64 > {name}.part 2>/dev/null || base64 -D -i {name}.b64 -o {name}.part 2>/dev/null; then
  mv {name}.part {name}
  rm -f {name}.b64
  printf '%suploaded %s/%s\n' "$__nori_m" "$PWD" {name}
else
  rm -f {name}.b64 {name}.part
  exit 4
fi
exit 0
}}
"#
    )
}

/// `text` as one single-quoted shell word.
pub(crate) fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// One line the script reported, read off stderr.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Report {
    Start { cwd: String },
    NoDir { dir: String },
    End { status: i64, cwd: String },
}

/// Split a stderr line into what the command printed (if anything) and a report. A report can
/// follow output that did not end in a newline, on the same line.
pub(crate) fn split_report(line: &str) -> (Option<&str>, Option<Report>) {
    let Some(at) = line.find(MARK) else {
        return (Some(line), None);
    };
    let before = (at > 0).then(|| &line[..at]);
    let report = &line[at + MARK.len()..];
    let parsed = if let Some(cwd) = report.strip_prefix("start cwd=") {
        Some(Report::Start {
            cwd: cwd.to_owned(),
        })
    } else if let Some(dir) = report.strip_prefix("nodir ") {
        Some(Report::NoDir {
            dir: dir.to_owned(),
        })
    } else if let Some(rest) = report.strip_prefix("end status=") {
        let (status, cwd) = rest.split_once(" cwd=").unwrap_or((rest, ""));
        status.parse().ok().map(|status| Report::End {
            status,
            cwd: cwd.to_owned(),
        })
    } else {
        None
    };
    match parsed {
        Some(report) => (before, Some(report)),
        None => (Some(line), None),
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn quoting_survives_single_quotes() {
        assert_eq!(quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn reports_are_split_from_output() {
        assert_eq!(
            split_report("partial\u{1e}nori:end status=3 cwd=/tmp/x y"),
            (
                Some("partial"),
                Some(Report::End {
                    status: 3,
                    cwd: "/tmp/x y".to_owned()
                })
            )
        );
        assert_eq!(split_report("plain"), (Some("plain"), None));
        assert_eq!(
            split_report("\u{1e}nori:start cwd=/home/me"),
            (
                None,
                Some(Report::Start {
                    cwd: "/home/me".to_owned()
                })
            )
        );
    }
}
