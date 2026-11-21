//! The `sh` scripts behind jobs. Every job lives in `~/.nori/jobs/ID` on its machine:
//!
//! - `cmd`: the command, run by bash (else `sh`) as a script file;
//! - `run`: the runner, a session leader of its own, so the job outlives Nori and the connection
//!   that started it and can be stopped as one process group;
//! - `out`, `err`: the job's output, appended (`>>`), so nothing is lost while nobody watches;
//! - `pid`: the runner's pid, which is also the job's process group; `boot`: the boot the job
//!   started in, so a pid after a restart is not mistaken for the job;
//! - `exit`: `exit N` or `signal NAME`, written once when the job ends; `cwd`: the directory
//!   the command ended in.
//!
//! The follower prints what the files gain, framed so bytes travel exactly:
//! `MARK chunk out|err OFFSET LENGTH`, a newline, then exactly LENGTH bytes; and at the end
//! `MARK exit exit N` (or `signal NAME`), or `MARK gone` when the job vanished without a word.

use crate::script::MARK;
use crate::script::quote;

/// The boot identity: Linux's boot id, or the boot time on a Mac.
const BOOT_ID: &str = r#"(cat /proc/sys/kernel/random/boot_id 2>/dev/null || sysctl -n kern.boottime 2>/dev/null | sed 's/.*sec = \([0-9]*\).*/\1/')"#;

/// The runner: starts the command with its output appended to the job's files, waits for it
/// through signals, and records how it ended.
const RUNNER: &str = r#"dir=$1
printf '%s\n' "$$" > "$dir/pid"
sig=
trap 'sig=TERM' TERM
trap 'sig=INT' INT
trap 'sig=HUP' HUP
"$NORI_JOB_SHELL" -c 'trap "pwd >\"\$0/cwd\" 2>/dev/null" EXIT; . "$0/cmd"' "$dir" </dev/null >>"$dir/out" 2>>"$dir/err" &
child=$!
while :; do
  wait "$child"
  status=$?
  kill -0 "$child" 2>/dev/null || break
done
if [ -n "$sig" ] && [ "$status" -gt 128 ]; then
  printf 'signal %s\n' "$sig" > "$dir/exit.tmp"
else
  printf 'exit %s\n' "$status" > "$dir/exit.tmp"
fi
mv "$dir/exit.tmp" "$dir/exit"
"#;

/// Where jobs live on a machine, as a shell word.
pub(crate) const DEFAULT_ROOT: &str = "\"$HOME\"/.nori/jobs";

/// Where a job's files are on its machine, as a shell word.
pub(crate) fn job_dir(root: &str, id: &str) -> String {
    format!("{root}/{}", quote(id))
}

/// Start a job, detached, and print `MARK job pid=PID boot=BOOT`.
pub(crate) fn launch(
    root: &str,
    id: &str,
    command: &str,
    cd: &str,
    env: &[(String, String)],
    path: Option<&str>,
) -> String {
    let dir = job_dir(root, id);
    let mut script = String::from("{\n");
    if let Some(path) = path.filter(|path| !path.is_empty()) {
        script.push_str(&format!("PATH={}; export PATH\n", quote(path)));
    }
    script.push_str(&format!("__nori_m=$(printf '\\036nori:')\ndir={dir}\n"));
    script.push_str(
        "mkdir -p \"$dir\" || { printf '%serror cannot create %s\\n' \"$__nori_m\" \"$dir\" >&2; exit 3; }\n",
    );
    script.push_str(cd);
    script.push_str(&format!("printf '%s' {} > \"$dir/cmd\"\n", quote(command)));
    script.push_str(&format!("printf '%s' {} > \"$dir/run\"\n", quote(RUNNER)));
    script.push_str(&format!("{BOOT_ID} > \"$dir/boot\"\n"));
    script.push_str("date +%s > \"$dir/started\"\n");
    for (key, value) in env {
        script.push_str(&format!("{key}={}; export {key}\n", quote(value)));
    }
    script.push_str(
        r#"NORI_JOB_SHELL=$(command -v bash 2>/dev/null || printf /bin/sh); export NORI_JOB_SHELL
if command -v setsid >/dev/null 2>&1; then
  setsid /bin/sh "$dir/run" "$dir" >/dev/null 2>&1 </dev/null &
elif command -v perl >/dev/null 2>&1; then
  perl -MPOSIX -e 'POSIX::setsid(); exec @ARGV' /bin/sh "$dir/run" "$dir" >/dev/null 2>&1 </dev/null &
else
  printf '%serror neither setsid nor perl is installed\n' "$__nori_m" >&2
  exit 3
fi
i=0
while [ ! -s "$dir/pid" ] && [ "$i" -lt 50 ]; do sleep 0.1; i=$((i+1)); done
printf '%sjob pid=%s boot=%s\n' "$__nori_m" "$(cat "$dir/pid" 2>/dev/null)" "$(cat "$dir/boot" 2>/dev/null)"
exit 0
}
"#,
    );
    script
}

/// Whether the job is still there: prints `MARK alive`, `MARK exit ...`, `MARK gone`, or
/// `MARK rebooted`.
pub(crate) fn check(root: &str, id: &str, pid: i64, boot: &str) -> String {
    let dir = job_dir(root, id);
    format!(
        r#"{{
__nori_m=$(printf '\036nori:')
dir={dir}
if [ -f "$dir/exit" ]; then printf '%sexit %s\n' "$__nori_m" "$(cat "$dir/exit")"; exit 0; fi
if [ "$({BOOT_ID})" != {boot} ]; then printf '%srebooted\n' "$__nori_m"; exit 0; fi
if kill -0 {pid} 2>/dev/null; then printf '%salive\n' "$__nori_m"; else printf '%sgone\n' "$__nori_m"; fi
exit 0
}}
"#,
        boot = quote(boot),
    )
}

/// Send `signal` to the job's process group.
pub(crate) fn signal(pid: i64, signal: &str) -> String {
    format!(
        "{{\nkill -{signal} -- -{pid} 2>/dev/null || kill -{signal} {pid} 2>/dev/null\nexit 0\n}}\n"
    )
}

/// TERM the job's process group, KILL it after `grace_tenths` tenths of a second, and record a
/// KILL if the job could not record its own end.
pub(crate) fn stop(root: &str, id: &str, pid: i64, grace_tenths: i64) -> String {
    let dir = job_dir(root, id);
    format!(
        r#"{{
dir={dir}
kill -TERM -- -{pid} 2>/dev/null || kill -TERM {pid} 2>/dev/null
i=0
while kill -0 {pid} 2>/dev/null && [ "$i" -lt {grace_tenths} ]; do sleep 0.1; i=$((i+1)); done
if kill -0 {pid} 2>/dev/null; then
  kill -KILL -- -{pid} 2>/dev/null || kill -KILL {pid} 2>/dev/null
  sleep 0.1
fi
[ -f "$dir/exit" ] || printf 'signal KILL\n' > "$dir/exit"
exit 0
}}
"#
    )
}

/// Print `MARK at OUT ERR` (the offsets it starts from; a negative offset means the stream's
/// current end), then, framed, what `out` and `err` gain, until the job ends. Hanging up stops
/// the follower, never the job.
pub(crate) fn follow(root: &str, id: &str, pid: i64, out_from: i64, err_from: i64) -> String {
    let dir = job_dir(root, id);
    let mark = MARK;
    format!(
        r#"{{
exec 3<&0
dir={dir}
( trap '' TERM; while IFS= read -r __l; do :; done; kill -TERM 0 2>/dev/null ) <&3 3<&- &
out={out_from}
err={err_from}
for s in out err; do
  eval "from=\$$s"
  if [ "$from" -lt 0 ]; then
    size=$(wc -c < "$dir/$s" 2>/dev/null || echo 0)
    eval "$s=$((size + 0))"
  fi
done
printf '{mark}at %s %s\n' "$out" "$err"
emit() {{
  size=$(wc -c < "$dir/$1" 2>/dev/null || echo 0)
  size=$((size + 0))
  eval "from=\$$1"
  if [ "$size" -gt "$from" ]; then
    len=$((size - from))
    printf '{mark}chunk %s %s %s\n' "$1" "$from" "$len"
    tail -c +"$((from + 1))" "$dir/$1" | head -c "$len"
    eval "$1=$size"
    return 0
  fi
  return 1
}}
while :; do
  ended=
  [ -f "$dir/exit" ] && ended=1
  grew=
  emit out && grew=1
  emit err && grew=1
  if [ -n "$ended" ] && [ -z "$grew" ]; then
    [ -f "$dir/cwd" ] && printf '{mark}cwd %s\n' "$(cat "$dir/cwd")"
    printf '{mark}exit %s\n' "$(cat "$dir/exit")"
    exit 0
  fi
  if [ -z "$ended" ] && [ -z "$grew" ] && ! kill -0 {pid} 2>/dev/null && [ ! -f "$dir/exit" ]; then
    printf '{mark}gone\n'
    exit 0
  fi
  [ -n "$grew" ] || sleep 0.2
done
}}
"#
    )
}

/// Print, framed like the follower, up to `max` bytes of each stream from the offsets.
pub(crate) fn read(root: &str, id: &str, out_from: i64, err_from: i64, max: i64) -> String {
    let dir = job_dir(root, id);
    let mark = MARK;
    format!(
        r#"{{
dir={dir}
for s in out:{out_from} err:{err_from}; do
  name=${{s%%:*}}; from=${{s#*:}}
  size=$(wc -c < "$dir/$name" 2>/dev/null || echo 0)
  size=$((size + 0))
  if [ "$from" -lt 0 ]; then from=$((size + from)); [ "$from" -lt 0 ] && from=0; fi
  len=$((size - from))
  [ "$len" -gt {max} ] && len={max}
  [ "$len" -gt 0 ] || continue
  printf '{mark}chunk %s %s %s\n' "$name" "$from" "$len"
  tail -c +"$((from + 1))" "$dir/$name" | head -c "$len"
done
exit 0
}}
"#
    )
}

/// Remove a job's files: a foreground command that ended is not a job anyone reads again.
pub(crate) fn forget(root: &str, id: &str) -> String {
    let dir = job_dir(root, id);
    format!("{{\nrm -rf {dir}\nexit 0\n}}\n")
}
