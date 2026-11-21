//! Files on a machine: stat, read, write, remove and list, over the same connection as its
//! commands, so file tools can work on another machine the way `bash` does.
//!
//! Bytes travel as base64 (in a here-document going there, on stdout coming back), so any file
//! survives the trip. A relative path is relative to the machine's home directory.

use std::io;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;

use base64::Engine as _;

use crate::Machine;
use crate::Roster;
#[cfg(test)]
use crate::exec::Connection;
#[cfg(test)]
use crate::exec::Via;
use crate::script::MARK;
use crate::script::RemoteDir;

/// How long one file operation may take.
const LIMIT: Duration = Duration::from_secs(60);

/// What a path is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    File,
    Dir,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathStat {
    pub kind: PathKind,
    pub len: i64,
    pub modified: Option<SystemTime>,
}

/// The files of one machine.
#[derive(Debug, Clone)]
pub struct MachineFs {
    reach: Reach,
}

#[derive(Debug, Clone)]
enum Reach {
    Machine(Arc<Roster>, Box<Machine>),
    #[cfg(test)]
    Local,
}

impl MachineFs {
    pub fn new(roster: Arc<Roster>, machine: Machine) -> Self {
        Self {
            reach: Reach::Machine(roster, Box::new(machine)),
        }
    }

    #[cfg(test)]
    pub(crate) fn local() -> Self {
        Self {
            reach: Reach::Local,
        }
    }

    pub fn machine(&self) -> Option<&Machine> {
        match &self.reach {
            Reach::Machine(_, machine) => Some(machine),
            #[cfg(test)]
            Reach::Local => None,
        }
    }

    async fn run(&self, script: &str) -> io::Result<Vec<u8>> {
        let connection = match &self.reach {
            Reach::Machine(roster, machine) => roster.connection(machine),
            #[cfg(test)]
            Reach::Local => Connection {
                machine: crate::name::local_name(),
                via: Via::Local,
            },
        };
        connection
            .run_short_bytes(script, LIMIT)
            .await
            .map_err(|error| io::Error::new(io::ErrorKind::ConnectionAborted, error.to_string()))
    }

    pub async fn stat(&self, path: &str) -> io::Result<PathStat> {
        let word = RemoteDir::Path(path.to_owned()).word();
        let script = format!(
            r#"{{
m=$(printf '\036nori:')
p={word}
if [ -d "$p" ]; then k=dir; elif [ -f "$p" ]; then k=file; elif [ -e "$p" ]; then k=other; else printf '%smissing\n' "$m"; exit 0; fi
len=0
[ "$k" = file ] && len=$(wc -c < "$p" 2>/dev/null || echo 0)
t=$(stat -c %Y "$p" 2>/dev/null || stat -f %m "$p" 2>/dev/null || echo)
printf '%sstat %s %s %s\n' "$m" "$k" "$((len + 0))" "$t"
exit 0
}}
"#
        );
        let out = String::from_utf8_lossy(&self.run(&script).await?).into_owned();
        let report = reported(&out)?;
        if report == "missing" {
            return Err(not_found(path));
        }
        let mut fields = report.strip_prefix("stat ").unwrap_or_default().split(' ');
        let kind = match fields.next() {
            Some("dir") => PathKind::Dir,
            Some("file") => PathKind::File,
            Some(_) | None => PathKind::Other,
        };
        let len = fields.next().and_then(|len| len.parse().ok()).unwrap_or(0);
        let modified = fields
            .next()
            .and_then(|seconds| seconds.trim().parse::<u64>().ok())
            .map(|seconds| SystemTime::UNIX_EPOCH + Duration::from_secs(seconds));
        Ok(PathStat {
            kind,
            len,
            modified,
        })
    }

    pub async fn read(&self, path: &str) -> io::Result<Vec<u8>> {
        let word = RemoteDir::Path(path.to_owned()).word();
        let script = format!(
            r#"{{
m=$(printf '\036nori:')
p={word}
if [ -d "$p" ]; then printf '%sisdir\n' "$m"; exit 0; fi
if [ ! -e "$p" ]; then printf '%smissing\n' "$m"; exit 0; fi
if [ ! -r "$p" ]; then printf '%sdenied\n' "$m"; exit 0; fi
printf '%sdata\n' "$m"
base64 < "$p"
exit 0
}}
"#
        );
        let out = self.run(&script).await?;
        let text = String::from_utf8_lossy(&out);
        let (head, body) = text.split_once('\n').unwrap_or((&text, ""));
        match head.strip_prefix(MARK) {
            Some("data") => {
                let encoded: String = body.chars().filter(|c| !c.is_whitespace()).collect();
                base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
            }
            Some("missing") => Err(not_found(path)),
            Some("isdir") => Err(io::Error::new(
                io::ErrorKind::IsADirectory,
                format!("{path} is a directory"),
            )),
            Some("denied") => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{path} is not readable"),
            )),
            Some(_) | None => Err(io::Error::other(format!("could not read {path}"))),
        }
    }

    /// Write the whole file through a temporary beside it and a rename, making its directory,
    /// keeping an existing file's permissions.
    pub async fn write(&self, path: &str, contents: &[u8]) -> io::Result<()> {
        let word = RemoteDir::Path(path.to_owned()).word();
        let encoded = base64::engine::general_purpose::STANDARD.encode(contents);
        let mut body = String::with_capacity(encoded.len() + encoded.len() / 76 + 1);
        for chunk in encoded.as_bytes().chunks(76) {
            body.push_str(&String::from_utf8_lossy(chunk));
            body.push('\n');
        }
        let script = format!(
            r#"{{
m=$(printf '\036nori:')
p={word}
mkdir -p "$(dirname "$p")" || {{ printf '%sfailed\n' "$m"; exit 0; }}
t="$p.nori-$$"
cat > "$t.b64" <<'NORI_FILE_6f1c'
{body}NORI_FILE_6f1c
if base64 -d < "$t.b64" > "$t" 2>/dev/null || base64 -D -i "$t.b64" -o "$t" 2>/dev/null; then
  rm -f "$t.b64"
  if [ -e "$p" ]; then
    mode=$(stat -c %a "$p" 2>/dev/null || stat -f %Lp "$p" 2>/dev/null)
    [ -n "$mode" ] && chmod "$mode" "$t"
  fi
  mv "$t" "$p" && printf '%swritten\n' "$m"
else
  rm -f "$t" "$t.b64"
  printf '%sfailed\n' "$m"
fi
exit 0
}}
"#
        );
        let out = String::from_utf8_lossy(&self.run(&script).await?).into_owned();
        match reported(&out)? {
            "written" => Ok(()),
            _ => Err(io::Error::other(format!("could not write {path}"))),
        }
    }

    pub async fn remove_file(&self, path: &str) -> io::Result<()> {
        let word = RemoteDir::Path(path.to_owned()).word();
        let script = format!(
            r#"{{
m=$(printf '\036nori:')
p={word}
if [ -d "$p" ]; then printf '%sisdir\n' "$m"; exit 0; fi
if [ ! -e "$p" ]; then printf '%smissing\n' "$m"; exit 0; fi
rm -f -- "$p" && printf '%sremoved\n' "$m"
exit 0
}}
"#
        );
        let out = String::from_utf8_lossy(&self.run(&script).await?).into_owned();
        match reported(&out)? {
            "removed" => Ok(()),
            "missing" => Err(not_found(path)),
            "isdir" => Err(io::Error::new(
                io::ErrorKind::IsADirectory,
                format!("{path} is a directory"),
            )),
            _ => Err(io::Error::other(format!("could not remove {path}"))),
        }
    }

    /// The entries of a directory, without `.` and `..`, sorted by name.
    pub async fn read_dir(&self, path: &str) -> io::Result<Vec<(String, PathKind)>> {
        let word = RemoteDir::Path(path.to_owned()).word();
        let script = format!(
            r#"{{
m=$(printf '\036nori:')
p={word}
if [ ! -d "$p" ]; then printf '%smissing\n' "$m"; exit 0; fi
printf '%slist\n' "$m"
for f in "$p"/* "$p"/.[!.]* "$p"/..?*; do
  [ -e "$f" ] || [ -L "$f" ] || continue
  n=${{f##*/}}
  if [ -d "$f" ]; then k=d; elif [ -f "$f" ]; then k=f; else k=o; fi
  printf '%s %s\0' "$k" "$n"
done
exit 0
}}
"#
        );
        let out = self.run(&script).await?;
        let text = String::from_utf8_lossy(&out);
        let (head, body) = text.split_once('\n').unwrap_or((&text, ""));
        match head.strip_prefix(MARK) {
            Some("list") => {
                let mut entries: Vec<(String, PathKind)> = body
                    .split('\0')
                    .filter_map(|entry| {
                        let (kind, name) = entry.split_once(' ')?;
                        let kind = match kind {
                            "d" => PathKind::Dir,
                            "f" => PathKind::File,
                            _ => PathKind::Other,
                        };
                        Some((name.to_owned(), kind))
                    })
                    .collect();
                entries.sort_by(|a, b| a.0.cmp(&b.0));
                Ok(entries)
            }
            Some("missing") => Err(not_found(path)),
            Some(_) | None => Err(io::Error::other(format!("could not list {path}"))),
        }
    }
}

fn reported(out: &str) -> io::Result<&str> {
    out.lines()
        .find_map(|line| line.strip_prefix(MARK))
        .ok_or_else(|| io::Error::other("the machine did not answer as expected"))
}

fn not_found(path: &str) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, format!("{path} does not exist"))
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[tokio::test]
    async fn files_go_there_and_come_back_byte_for_byte() {
        let dir = tempfile::tempdir().unwrap_or_else(|error| unreachable!("{error}"));
        let fs = MachineFs::local();
        let path = dir.path().join("nested/it's a file.bin");
        let path = path.display().to_string();
        let bytes: Vec<u8> = (0..=255)
            .chain(b"\nNORI_FILE_6f1c\n".iter().copied())
            .collect();
        fs.write(&path, &bytes)
            .await
            .unwrap_or_else(|error| unreachable!("write: {error}"));
        assert_eq!(fs.read(&path).await.ok(), Some(bytes.clone()));
        let stat = fs.stat(&path).await.ok();
        assert_eq!(
            stat.map(|stat| (stat.kind, stat.len)),
            Some((PathKind::File, bytes.len() as i64))
        );
        let listing = fs
            .read_dir(&dir.path().join("nested").display().to_string())
            .await
            .ok();
        assert_eq!(
            listing,
            Some(vec![("it's a file.bin".to_owned(), PathKind::File)])
        );
        fs.remove_file(&path)
            .await
            .unwrap_or_else(|error| unreachable!("remove: {error}"));
        assert_eq!(
            fs.read(&path).await.map_err(|error| error.kind()),
            Err(io::ErrorKind::NotFound)
        );
    }
}
