//! The single error type for this crate.

/// Errors originating from `yamaha-rcp`.
///
/// Every variant is `Send + Sync` so an error can cross a task boundary — the previous
/// `Box<dyn std::error::Error>` payload was neither, which made the type unusable in the
/// spawned tasks that actually drive the connection.
#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum Error {
    /// The TCP connection failed, or was not established.
    #[error("network error: {0}")]
    Network(#[from] std::io::Error),

    /// The configured console address could not be parsed.
    #[error("invalid network address: {0}")]
    AddrParse(#[from] std::net::AddrParseError),

    /// The console replied `ERROR` to a command.
    #[error("console rejected command: {0}")]
    Console(String),

    /// A reply arrived but did not match the shape expected for the command.
    #[error("could not parse console response {response:?}: {reason}")]
    Parse { response: String, reason: String },

    /// No reply arrived within the configured timeout.
    ///
    /// The command may still have been applied — RCP gives no way to distinguish a lost reply
    /// from a lost command.
    #[error("console did not reply to {command:?} within {timeout_ms}ms")]
    Timeout { command: String, timeout_ms: u64 },

    /// The connection actor has stopped, so no further commands can be sent.
    #[error("connection to the console is closed")]
    Disconnected,

    /// The parameter path is not in the console's parameter table.
    #[error("unknown parameter path: {0}")]
    UnknownParam(String),

    /// The channel or send index is outside the range the parameter table allows.
    #[error("{path}: {what} index {index} out of range (valid: 0..{count})")]
    IndexOutOfRange {
        path: String,
        what: &'static str,
        index: u32,
        count: u32,
    },

    /// The parameter exists but the console will not accept a write to it.
    #[error("parameter is read-only: {0}")]
    ReadOnly(String),

    /// A `LabelColor` string did not name a known colour.
    #[error("unknown label colour: {0}")]
    LabelColorParse(String),

    /// A `SceneList` string did not name a known scene list.
    #[error("unknown scene list: {0}")]
    SceneListParse(String),
}

impl Error {
    /// Build a [`Error::Parse`] from the offending response and a reason.
    pub(crate) fn parse(response: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::Parse {
            response: response.into(),
            reason: reason.into(),
        }
    }

    /// Whether retrying the same command could plausibly succeed.
    ///
    /// A rejected or malformed command will be rejected again; a timeout or a dropped connection
    /// may not be. Callers driving a show use this to decide whether to re-send.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::Network(_) | Self::Timeout { .. } | Self::Disconnected
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of reshaping this enum: it has to survive being moved between tasks.
    #[test]
    fn error_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync + 'static>() {}
        assert_send_sync::<Error>();
    }

    #[test]
    fn transient_errors_are_the_retryable_ones() {
        assert!(Error::Disconnected.is_transient());
        assert!(Error::Timeout {
            command: "get x 0 0".into(),
            timeout_ms: 500
        }
        .is_transient());
        assert!(!Error::Console("ERROR set".into()).is_transient());
        assert!(!Error::ReadOnly("MIXER:Current/InCh/Role".into()).is_transient());
    }
}
