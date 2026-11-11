use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A rejected identifier, with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidId {
    pub kind: &'static str,
    pub value: String,
    pub reason: &'static str,
}

impl fmt::Display for InvalidId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid {} {:?}: {}", self.kind, self.value, self.reason)
    }
}

impl std::error::Error for InvalidId {}

macro_rules! string_id {
    ($(#[$doc:meta])* $name:ident, $kind:literal, $check:path) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = InvalidId;

            fn try_from(value: String) -> Result<Self, InvalidId> {
                match $check(&value) {
                    Ok(()) => Ok(Self(value)),
                    Err(reason) => Err(InvalidId { kind: $kind, value, reason }),
                }
            }
        }

        impl FromStr for $name {
            type Err = InvalidId;

            fn from_str(value: &str) -> Result<Self, InvalidId> {
                Self::try_from(value.to_owned())
            }
        }

        impl From<$name> for String {
            fn from(id: $name) -> String {
                id.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

string_id!(
    /// A board's slug: 1–40 characters of `a-z`, `0-9` and `-`, starting with a letter or
    /// digit. Example: `factory`.
    BoardSlug,
    "board slug",
    check_slug
);

string_id!(
    /// A task id: `<board slug>-<number>`, for example `factory-12`. Numbers start at 1 per
    /// board and are never reused.
    TaskId,
    "task id",
    check_task_id
);

string_id!(
    /// An agent (thread) id: `<source>:<native id>`, where the source is `t3` (a T3 Code
    /// thread), `claude` (a standalone Claude Code session), `codex` (a standalone Codex
    /// session) or `dispatch` (a device daemon that dispatches a board's work, named after
    /// its device: `dispatch:<device name>`). The native id is 1–256 characters of
    /// `A-Z a-z 0-9 . _ - : %`; a `dispatch` one is a [`DeviceName`].
    AgentId,
    "agent id",
    check_agent_id
);

string_id!(
    /// A device name: 1–40 characters of `a-z`, `0-9` and `-`. Example: `mac-air`.
    DeviceName,
    "device name",
    check_slug
);

/// Where an agent id comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentSource {
    T3,
    Claude,
    Codex,
    Opencode,
    /// A device daemon dispatching a board's work (`docs/API.md`, "Daemon dispatch").
    Dispatch,
}

impl AgentId {
    pub fn source(&self) -> AgentSource {
        match self.0.split_once(':').map(|(source, _)| source) {
            Some("t3") => AgentSource::T3,
            Some("claude") => AgentSource::Claude,
            Some("dispatch") => AgentSource::Dispatch,
            Some("opencode") => AgentSource::Opencode,
            _ => AgentSource::Codex,
        }
    }

    pub fn native(&self) -> &str {
        self.0.split_once(':').map_or("", |(_, native)| native)
    }

    /// The dispatch agent of `device`: `dispatch:<device>`.
    pub fn dispatch(device: &DeviceName) -> Self {
        Self(format!("dispatch:{device}"))
    }

    /// The device a dispatch agent is named after; `None` for other sources.
    pub fn dispatch_device(&self) -> Option<DeviceName> {
        (self.source() == AgentSource::Dispatch).then(|| DeviceName(self.native().to_owned()))
    }
}

impl TaskId {
    pub fn new(board: &BoardSlug, number: u64) -> Self {
        Self(format!("{board}-{number}"))
    }

    pub fn board(&self) -> BoardSlug {
        let (board, _) = self
            .0
            .rsplit_once('-')
            .expect("validated task id has a dash");
        BoardSlug(board.to_owned())
    }

    pub fn number(&self) -> u64 {
        let (_, number) = self
            .0
            .rsplit_once('-')
            .expect("validated task id has a dash");
        number.parse().expect("validated task id ends in a number")
    }
}

fn check_slug(value: &str) -> Result<(), &'static str> {
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes.len() > 40 {
        return Err("must be 1-40 characters");
    }
    if !bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
    {
        return Err("only a-z, 0-9 and - are allowed");
    }
    if bytes[0] == b'-' {
        return Err("must start with a letter or digit");
    }
    Ok(())
}

fn check_task_id(value: &str) -> Result<(), &'static str> {
    let (board, number) = value.rsplit_once('-').ok_or("must be <board>-<number>")?;
    check_slug(board)?;
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) || number.starts_with('0') {
        return Err("must end in a number without leading zeros");
    }
    number
        .parse::<u64>()
        .map(|_| ())
        .map_err(|_| "number is too large")
}

fn check_agent_id(value: &str) -> Result<(), &'static str> {
    let (source, native) = value
        .split_once(':')
        .ok_or("must be <source>:<native id>")?;
    if !matches!(source, "t3" | "claude" | "codex" | "opencode" | "dispatch") {
        return Err("source must be t3, claude, codex, opencode or dispatch");
    }
    if source == "dispatch" {
        return check_slug(native)
            .map_err(|_| "a dispatch agent is dispatch:<device name> (a-z, 0-9 and -)");
    }
    if native.is_empty() || native.len() > 256 {
        return Err("native id must be 1-256 characters");
    }
    let allowed =
        |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':' | b'%');
    if !native.bytes().all(allowed) {
        return Err("native id allows only A-Z a-z 0-9 . _ - : %");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_ids_round_trip_and_split() {
        let id: TaskId = "agent-board-12".parse().unwrap();
        assert_eq!(id.board().as_str(), "agent-board");
        assert_eq!(id.number(), 12);
        assert_eq!(
            TaskId::new(&"factory".parse().unwrap(), 3).as_str(),
            "factory-3"
        );
    }

    #[test]
    fn malformed_ids_are_rejected() {
        for bad in [
            "factory",
            "factory-",
            "factory-01",
            "-1",
            "Factory-1",
            "factory-x",
        ] {
            assert!(bad.parse::<TaskId>().is_err(), "{bad}");
        }
        for bad in ["", "-a", "a b", "A", &"a".repeat(41)] {
            assert!(bad.parse::<BoardSlug>().is_err(), "{bad}");
        }
        for bad in ["t3", "t3:", "gpt:1", "t3:a b", "t3:<x>"] {
            assert!(bad.parse::<AgentId>().is_err(), "{bad}");
        }
    }

    #[test]
    fn agent_ids_accept_t3_child_thread_ids() {
        let id: AgentId = "t3:thread:delegated-task:command%3Amcp%3A47f9"
            .parse()
            .unwrap();
        assert_eq!(id.source(), AgentSource::T3);
        assert_eq!(id.native(), "thread:delegated-task:command%3Amcp%3A47f9");
    }

    #[test]
    fn opencode_agent_ids_have_their_own_source() {
        let id: AgentId = "opencode:ses_abc123".parse().unwrap();
        assert_eq!(id.source(), AgentSource::Opencode);
        assert_eq!(id.native(), "ses_abc123");
    }

    #[test]
    fn dispatch_agents_are_named_after_their_device() {
        let device: DeviceName = "mac-pro".parse().unwrap();
        let id = AgentId::dispatch(&device);
        assert_eq!(id.as_str(), "dispatch:mac-pro");
        assert_eq!(id.source(), AgentSource::Dispatch);
        assert_eq!(id.dispatch_device(), Some(device));
        assert_eq!("dispatch:mac-pro".parse::<AgentId>().unwrap(), id);
        let t3: AgentId = "t3:abc".parse().unwrap();
        assert_eq!(t3.dispatch_device(), None);
        for bad in ["dispatch:", "dispatch:Mac", "dispatch:a:b", "dispatch:-a"] {
            assert!(bad.parse::<AgentId>().is_err(), "{bad}");
        }
    }

    #[test]
    fn deserializing_validates() {
        assert!(serde_json::from_str::<TaskId>("\"factory-0\"").is_err());
        let id: AgentId = serde_json::from_str("\"claude:abc-123\"").unwrap();
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"claude:abc-123\"");
    }
}
