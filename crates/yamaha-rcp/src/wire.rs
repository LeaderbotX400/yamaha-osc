//! Tokenising and framing for the RCP text protocol.
//!
//! RCP is a line protocol: each message is a `\n`-terminated line of space-separated tokens, where
//! a token may be a bare word or a double-quoted string that can itself contain spaces. There are
//! no request IDs, so correlation is purely positional — one response per request, in order.

/// Split an RCP line into tokens, keeping quoted strings (which may contain spaces) as one token.
///
/// Quotes are stripped from the returned tokens; use [`is_quoted`] beforehand if you need to know
/// whether a token was quoted. This is the Rust equivalent of the `(?:[^\s"]+|"[^"]*")+` regex the
/// Bitfocus Companion module uses, which is the only widely-exercised parser for this protocol.
///
/// ```
/// use yamaha_rcp::wire::tokenize;
///
/// let t = tokenize(r#"OK get MIXER:Current/InCh/Label/Name 0 0 "Lead Vox""#);
/// assert_eq!(t, ["OK", "get", "MIXER:Current/InCh/Label/Name", "0", "0", "Lead Vox"]);
/// ```
pub fn tokenize(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut has_token = false;

    for c in line.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                has_token = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if has_token {
                    tokens.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            c => {
                current.push(c);
                has_token = true;
            }
        }
    }

    if has_token {
        tokens.push(current);
    }

    tokens
}

/// Quote and escape a string for use as an RCP argument.
///
/// The console has no documented escape syntax, so an embedded `"` cannot be represented; it is
/// stripped rather than sent, because an unbalanced quote would desynchronise the console's parser
/// for the rest of the line.
pub fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    out.extend(s.chars().filter(|&c| c != '"' && c != '\n' && c != '\r'));
    out.push('"');
    out
}

/// The leading status word of a message from the console.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Request succeeded.
    Ok,
    /// Request succeeded, but the console clamped or otherwise modified the value.
    OkModified,
    /// Request failed.
    Error,
    /// Unsolicited notification of a change made elsewhere (console surface, another client).
    Notify,
}

impl Status {
    /// Classify the first token of a console message, if it is a status word.
    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "OK" => Some(Self::Ok),
            "OKm" => Some(Self::OkModified),
            "ERROR" => Some(Self::Error),
            "NOTIFY" => Some(Self::Notify),
            _ => None,
        }
    }

    /// Whether this status completes an outstanding request.
    ///
    /// `NOTIFY` does not: the console pushes notifications unsolicited, so treating one as a
    /// response would misalign every subsequent request with its reply.
    pub fn is_response(self) -> bool {
        !matches!(self, Self::Notify)
    }
}

/// Incremental `\n` line splitter for a byte stream.
///
/// A single TCP read is not a message boundary — a response can arrive split across reads, or
/// several responses can arrive in one. This buffers the remainder between reads so neither case
/// loses data.
#[derive(Debug, Default)]
pub struct LineBuffer {
    buf: Vec<u8>,
}

impl LineBuffer {
    /// Maximum bytes to retain for an unterminated line before treating the peer as desynchronised.
    /// The longest legitimate RCP line is a 64-character label plus its path, comfortably under 1 KiB.
    const MAX_LINE: usize = 8192;

    pub fn new() -> Self {
        Self::default()
    }

    /// Append received bytes and return every complete line they finish.
    ///
    /// Lines are decoded lossily: the console's character set for labels is not documented, and a
    /// mangled label is far better than a panic or a dropped connection.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, Desync> {
        self.buf.extend_from_slice(bytes);

        let mut lines = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
            line.pop(); // trailing \n
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let text = String::from_utf8_lossy(&line).trim().to_string();
            if !text.is_empty() {
                lines.push(text);
            }
        }

        if self.buf.len() > Self::MAX_LINE {
            let len = self.buf.len();
            self.buf.clear();
            return Err(Desync(len));
        }

        Ok(lines)
    }

    /// Discard any partial line. Called on reconnect so stale bytes can't prefix a fresh response.
    pub fn reset(&mut self) {
        self.buf.clear();
    }
}

/// The peer sent more than [`LineBuffer::MAX_LINE`] bytes without a newline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Desync(pub usize);

impl std::fmt::Display for Desync {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} bytes received without a line terminator", self.0)
    }
}

impl std::error::Error for Desync {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_plain() {
        assert_eq!(
            tokenize("OK set MIXER:Current/InCh/Fader/Level 0 0 -1000"),
            [
                "OK",
                "set",
                "MIXER:Current/InCh/Fader/Level",
                "0",
                "0",
                "-1000"
            ]
        );
    }

    #[test]
    fn tokenize_keeps_spaces_inside_quotes() {
        assert_eq!(
            tokenize(r#"OK get MIXER:Current/InCh/Label/Name 4 0 "Lead  Vox""#),
            [
                "OK",
                "get",
                "MIXER:Current/InCh/Label/Name",
                "4",
                "0",
                "Lead  Vox"
            ]
        );
    }

    #[test]
    fn tokenize_keeps_empty_quoted_string_as_a_token() {
        // An empty label is a real value and must not vanish, or the argument
        // positions after it shift and the response is misread.
        let t = tokenize(r#"OK get MIXER:Current/InCh/Label/Name 4 0 """#);
        assert_eq!(t.len(), 6);
        assert_eq!(t[5], "");
    }

    #[test]
    fn tokenize_handles_runs_of_whitespace() {
        assert_eq!(tokenize("OK   get    x"), ["OK", "get", "x"]);
    }

    #[test]
    fn quote_strips_embedded_quotes() {
        assert_eq!(quote(r#"He said "hi""#), r#""He said hi""#);
    }

    #[test]
    fn quote_strips_newlines_so_one_command_stays_one_line() {
        assert_eq!(quote("a\nb"), r#""ab""#);
    }

    #[test]
    fn line_buffer_joins_a_response_split_across_reads() {
        // Regression: the original reader allocated a fresh line buffer per TCP read, so a
        // response split across two reads was silently dropped and the caller hung forever.
        let mut lb = LineBuffer::new();
        assert!(lb.push(b"OK get MIXER:Curr").unwrap().is_empty());
        assert_eq!(
            lb.push(b"ent/InCh/Fader/Level 0 0 -1000\n").unwrap(),
            ["OK get MIXER:Current/InCh/Fader/Level 0 0 -1000"]
        );
    }

    #[test]
    fn line_buffer_splits_several_responses_in_one_read() {
        let mut lb = LineBuffer::new();
        assert_eq!(
            lb.push(b"OK a\nOK b\nOK c\n").unwrap(),
            ["OK a", "OK b", "OK c"]
        );
    }

    #[test]
    fn line_buffer_ignores_nul_padding_and_blank_lines() {
        let mut lb = LineBuffer::new();
        assert_eq!(lb.push(b"OK a\n\n\n").unwrap(), ["OK a"]);
    }

    #[test]
    fn line_buffer_survives_invalid_utf8() {
        // Regression: the original reader called from_utf8(..).unwrap() inside a detached task,
        // so one bad byte from the console aborted the reader with no way to observe it.
        let mut lb = LineBuffer::new();
        let lines = lb.push(b"OK get \xff\xfe name\n").unwrap();
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn line_buffer_reports_desync_instead_of_growing_without_bound() {
        let mut lb = LineBuffer::new();
        assert_eq!(lb.push(&vec![b'x'; 9000]), Err(Desync(9000)));
    }

    #[test]
    fn status_notify_is_not_a_response() {
        assert!(!Status::from_token("NOTIFY").unwrap().is_response());
        assert!(Status::from_token("OK").unwrap().is_response());
        assert!(Status::from_token("OKm").unwrap().is_response());
        assert!(Status::from_token("ERROR").unwrap().is_response());
        assert_eq!(Status::from_token("MIXER:Current/InCh"), None);
    }
}
