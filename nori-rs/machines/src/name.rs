//! The names machines go by, and the ssh destinations that reach them.

use std::fmt;
use std::str::FromStr;

use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;

/// What the agent and the user call a machine: a Rex host label or a `[machines.NAME]` key.
///
/// Letters, digits, `.`, `_` and `-`, at most 64 bytes, and never `local`, which always means
/// the computer Nori runs on.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MachineName(String);

/// The name that always means the computer Nori runs on.
pub const LOCAL: &str = "local";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    #[error("a machine name cannot be empty")]
    Empty,
    #[error("a machine name is at most 64 bytes")]
    TooLong,
    #[error("`{0}` is not a machine name: use letters, digits, `.`, `_` and `-`")]
    Invalid(String),
    #[error("`local` always means this computer and cannot name another machine")]
    Local,
}

impl MachineName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What errors call this computer when a job or command on it fails.
pub(crate) fn local_name() -> &'static MachineName {
    static NAME: std::sync::OnceLock<MachineName> = std::sync::OnceLock::new();
    NAME.get_or_init(|| MachineName("this-computer".to_owned()))
}

impl FromStr for MachineName {
    type Err = NameError;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        if name.is_empty() {
            return Err(NameError::Empty);
        }
        if name.len() > 64 {
            return Err(NameError::TooLong);
        }
        if name.eq_ignore_ascii_case(LOCAL) {
            return Err(NameError::Local);
        }
        let valid = name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
            && !name.starts_with('-');
        if !valid {
            return Err(NameError::Invalid(name.to_owned()));
        }
        Ok(Self(name.to_owned()))
    }
}

impl fmt::Display for MachineName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for MachineName {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for MachineName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

/// What `ssh` is given to reach a machine: `host`, `user@host`, or an alias from the user's
/// ssh config. Never starts with `-`, so it cannot be read as an option.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SshDestination(String);

impl SshDestination {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// What a connection that never leaves this computer is called in errors.
    pub(crate) fn this_computer() -> Self {
        Self("this computer".to_owned())
    }

    /// The destination of an `ssh://[user@]host[:port]` URL, as Rex stores a host's endpoint.
    /// A port is not representable as a destination, so such a URL is refused.
    pub fn from_ssh_url(url: &str) -> Option<Self> {
        let rest = url.strip_prefix("ssh://")?;
        let rest = rest.trim_end_matches('/');
        let host = rest.rsplit_once('@').map_or(rest, |(_, host)| host);
        if (host.starts_with('[') && !host.ends_with(']'))
            || (!host.starts_with('[') && host.contains(':'))
        {
            return None;
        }
        rest.parse().ok()
    }
}

impl FromStr for SshDestination {
    type Err = String;

    fn from_str(destination: &str) -> Result<Self, Self::Err> {
        let destination = destination.trim();
        if destination.is_empty() {
            return Err("an ssh destination cannot be empty".to_owned());
        }
        if destination.starts_with('-') {
            return Err(format!("`{destination}` is not an ssh destination"));
        }
        if destination
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(format!("`{destination}` is not an ssh destination"));
        }
        Ok(Self(destination.to_owned()))
    }
}

impl fmt::Display for SshDestination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn names_are_labels_and_never_local() {
        assert_eq!(
            "h16003mac".parse::<MachineName>().map(|n| n.to_string()),
            Ok("h16003mac".to_owned())
        );
        assert_eq!("Local".parse::<MachineName>(), Err(NameError::Local));
        assert_eq!(
            "a b".parse::<MachineName>(),
            Err(NameError::Invalid("a b".to_owned()))
        );
        assert_eq!(
            "-oProxyCommand".parse::<MachineName>(),
            Err(NameError::Invalid("-oProxyCommand".to_owned()))
        );
    }

    #[test]
    fn rex_ssh_endpoints_become_destinations() {
        assert_eq!(
            SshDestination::from_ssh_url("ssh://rmccrar6@h16003mac.tail7f9012.ts.net"),
            Some(SshDestination(
                "rmccrar6@h16003mac.tail7f9012.ts.net".to_owned()
            ))
        );
        assert_eq!(SshDestination::from_ssh_url("ssh://box:2222"), None);
        assert_eq!(SshDestination::from_ssh_url("ssh://[::1]:2222"), None);
        assert!(SshDestination::from_ssh_url("ssh://[::1]").is_some());
        assert_eq!(SshDestination::from_ssh_url("https://rex-box.ts.net"), None);
        assert!("-oProxyCommand=x".parse::<SshDestination>().is_err());
    }
}
