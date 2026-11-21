//! Asking the terminal whether it understands OSC 7501.
//!
//! The question goes out together with primary device attributes, which every terminal answers;
//! the replies come back in order, so a device-attributes reply with no status reply before it
//! means the terminal does not know the protocol. The replies are read straight from the
//! controlling terminal, before the event reader exists, so they never reach the composer as
//! keystrokes.

use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::time::Duration;
use std::time::Instant;

use super::wire::Capabilities;
use super::wire::QUERY;

/// A terminal that has answered neither query by now is not going to.
const TIMEOUT: Duration = Duration::from_millis(250);

/// The terminal's capabilities, or `None` when it does not support the protocol.
pub(super) fn query() -> Option<Capabilities> {
    let mut tty = File::open("/dev/tty").ok()?;
    {
        let mut out = std::io::stdout().lock();
        out.write_all(QUERY.as_bytes()).ok()?;
        out.flush().ok()?;
    }
    let deadline = Instant::now() + TIMEOUT;
    let mut seen = Vec::new();
    let mut chunk = [0u8; 256];
    loop {
        let remaining = deadline.checked_duration_since(Instant::now())?;
        if !readable(tty.as_raw_fd(), remaining) {
            continue;
        }
        let read = tty.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        seen.extend_from_slice(&chunk[..read]);
        if let Some(caps) = status_reply(&seen) {
            return Some(caps);
        }
        if device_attributes_reply(&seen) {
            return None;
        }
    }
}

fn readable(fd: i32, timeout: Duration) -> bool {
    // `select`, not `poll`: poll does not work on a terminal device on macOS.
    let mut tv = libc::timeval {
        tv_sec: timeout.as_secs() as _,
        tv_usec: i64::from(timeout.subsec_micros()) as _,
    };
    // SAFETY: `set` is initialized by FD_ZERO before use and `fd` is a live descriptor below
    // FD_SETSIZE; every pointer passed to `select` outlives the call.
    unsafe {
        let mut set: libc::fd_set = std::mem::zeroed();
        libc::FD_ZERO(&mut set);
        libc::FD_SET(fd, &mut set);
        libc::select(
            fd + 1,
            &mut set,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut tv,
        ) > 0
    }
}

/// The capabilities in the first complete `OSC 7501 ; ? ... ST` in `bytes`.
fn status_reply(bytes: &[u8]) -> Option<Capabilities> {
    const START: &[u8] = b"\x1b]7501;";
    let start = bytes.windows(START.len()).position(|part| part == START)? + START.len();
    let body = &bytes[start..];
    let end = body.iter().position(|&byte| byte == 0x07 || byte == 0x1b)?;
    Capabilities::parse_reply(std::str::from_utf8(&body[..end]).ok()?)
}

/// Whether a complete `CSI ? ... c` is in `bytes`.
fn device_attributes_reply(bytes: &[u8]) -> bool {
    const START: &[u8] = b"\x1b[?";
    let Some(start) = bytes.windows(START.len()).position(|part| part == START) else {
        return false;
    };
    bytes[start + START.len()..]
        .iter()
        .find(|byte| (0x40..=0x7e).contains(*byte))
        == Some(&b'c')
}
