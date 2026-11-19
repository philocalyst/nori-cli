//! Read OpenCode's local service registration without starting a process.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::path::PathBuf;

use serde_json::Value;

pub(crate) struct Registration {
    pub url: String,
    pub password: String,
    pub pid: i64,
    pub version: Option<String>,
}

impl Registration {
    /// Whether the process that wrote the registration still exists. A registration outlives a
    /// server that was killed, so a dead process means the URL is stale. Answers `true` when it
    /// cannot tell (another user's process, or a platform without a cheap check).
    pub(crate) fn process_is_running(&self) -> bool {
        #[cfg(unix)]
        {
            let Ok(pid) = i32::try_from(self.pid) else {
                return true;
            };
            // Signal 0 only checks that the process can be signalled.
            let gone = unsafe { libc::kill(pid, 0) } == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
            !gone
        }
        #[cfg(not(unix))]
        {
            true
        }
    }
}

pub(crate) fn read(environment: &HashMap<OsString, OsString>) -> Option<Registration> {
    let variable = |name: &str| {
        environment
            .get(OsStr::new(name))
            .cloned()
            .or_else(|| std::env::var_os(name))
            .filter(|value| !value.is_empty())
    };
    let state = variable("XDG_STATE_HOME").map(PathBuf::from).or_else(|| {
        variable("HOME")
            .or_else(|| variable("USERPROFILE"))
            .map(|home| PathBuf::from(home).join(".local").join("state"))
    })?;
    let contents = std::fs::read(state.join("opencode").join("service.json")).ok()?;
    parse(&contents)
}

fn parse(contents: &[u8]) -> Option<Registration> {
    let value: Value = serde_json::from_slice(contents).ok()?;
    let url = value.get("url")?.as_str()?;
    let parsed = reqwest::Url::parse(url).ok()?;
    // Do not send the local service's credential to a remote or user-specified URL.
    let host = parsed.host_str()?;
    let is_loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    if !matches!(parsed.scheme(), "http" | "https")
        || !is_loopback
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return None;
    }
    let pid = value.get("pid")?.as_i64().filter(|pid| *pid > 0)?;
    Some(Registration {
        url: url.to_owned(),
        password: value
            .get("password")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        pid,
        version: value
            .get("version")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
    })
}

#[cfg(test)]
mod tests {
    use super::parse;
    use pretty_assertions::assert_eq;

    #[test]
    fn accepts_local_registration_and_rejects_remote_or_malformed_entries() {
        let info = parse(
            br#"{"url":"http://127.0.0.1:1234","pid":42,"version":"2.1.4","password":"test"}"#,
        )
        .unwrap();
        assert_eq!(
            (
                info.url.as_str(),
                info.pid,
                info.version.as_deref(),
                info.password.as_str()
            ),
            ("http://127.0.0.1:1234", 42, Some("2.1.4"), "test")
        );
        for body in [
            r#"{"url":"http://example.com:1234","pid":42}"#,
            r#"{"url":"http://user@localhost:1234","pid":42}"#,
            r#"{"url":"http://localhost:1234","pid":0}"#,
            r#"{"url":"http://localhost:1234"}"#,
        ] {
            assert!(parse(body.as_bytes()).is_none());
        }
    }
}
