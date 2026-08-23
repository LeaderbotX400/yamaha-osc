//! An in-process fake Yamaha console that speaks RCP over TCP.
//!
//! Nobody can keep a TF1 on a laptop, so the protocol behaviour that matters most — replies split
//! across TCP segments, the console hanging up mid-show, a reply that never arrives — has to be
//! reproducible without hardware. This mock exists to make those cases testable, and every fault
//! it can inject corresponds to a real failure the previous implementation handled badly.
//!
//! ```no_run
//! # async fn demo() {
//! let console = rcp_mock::MockConsole::start().await;
//! // point a client at console.addr() ...
//! assert_eq!(console.commands().len(), 0);
//! # }
//! ```

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;

/// Faults the mock can inject, each mirroring a real-world failure mode.
#[derive(Clone, Debug, Default)]
pub struct Faults {
    /// Write every reply in two TCP segments with a gap between them, so a client that assumes
    /// one read == one message loses the reply.
    pub split_responses: bool,
    /// Silently ignore this many commands instead of replying, simulating a lost reply.
    pub swallow_replies: usize,
    /// Close the connection after this many commands have been received.
    pub hangup_after: Option<usize>,
    /// Reply `ERROR` for commands touching any of these parameter paths.
    pub error_paths: HashSet<String>,
}

#[derive(Debug, Default)]
struct Shared {
    /// Every command line received, across all connections, in order.
    received: Vec<String>,
    /// Parameter store: (path, index, sub) -> the value text exactly as it was set.
    values: HashMap<(String, u32, u32), String>,
    faults: Faults,
    connections: usize,
}

/// A running fake console. Dropping it shuts the listener down.
#[derive(Debug)]
pub struct MockConsole {
    addr: SocketAddr,
    shared: Arc<Mutex<Shared>>,
    /// Broadcast to every client task: `Some(line)` pushes a NOTIFY, `None` hangs up.
    control: broadcast::Sender<Option<String>>,
}

/// Builds a [`MockConsole`] with faults configured.
#[derive(Debug, Default)]
pub struct MockConsoleBuilder {
    faults: Faults,
}

impl MockConsoleBuilder {
    /// Split each reply across two TCP segments.
    pub fn split_responses(mut self) -> Self {
        self.faults.split_responses = true;
        self
    }

    /// Ignore the next `n` commands rather than replying to them.
    pub fn swallow_replies(mut self, n: usize) -> Self {
        self.faults.swallow_replies = n;
        self
    }

    /// Close the connection once `n` commands have been received.
    pub fn hangup_after(mut self, n: usize) -> Self {
        self.faults.hangup_after = Some(n);
        self
    }

    /// Reply `ERROR` to any command mentioning `path`.
    pub fn error_on(mut self, path: impl Into<String>) -> Self {
        self.faults.error_paths.insert(path.into());
        self
    }

    /// Bind a listener on localhost and start serving.
    pub async fn start(self) -> MockConsole {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock console must be able to bind a loopback port");
        let addr = listener
            .local_addr()
            .expect("bound listener has an address");

        let shared = Arc::new(Mutex::new(Shared {
            faults: self.faults,
            ..Shared::default()
        }));
        let (control, _) = broadcast::channel(64);

        tokio::spawn(accept_loop(listener, Arc::clone(&shared), control.clone()));

        MockConsole {
            addr,
            shared,
            control,
        }
    }
}

impl MockConsole {
    /// Start a console with no faults.
    pub async fn start() -> Self {
        Self::builder().start().await
    }

    /// Configure faults before starting.
    pub fn builder() -> MockConsoleBuilder {
        MockConsoleBuilder::default()
    }

    /// The loopback address to point a client at.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Every command line received so far, in order.
    pub fn commands(&self) -> Vec<String> {
        self.lock().received.clone()
    }

    /// Commands received so far, excluding the keepalive traffic every session opens with, so
    /// assertions can talk about what the test actually sent.
    pub fn commands_excluding_keepalive(&self) -> Vec<String> {
        self.lock()
            .received
            .iter()
            .filter(|c| !c.starts_with("scpmode keepalive") && *c != "devstatus runmode")
            .cloned()
            .collect()
    }

    /// How many connections have been accepted. Used to assert that a client reconnected.
    pub fn connection_count(&self) -> usize {
        self.lock().connections
    }

    /// The stored value for a parameter, as the text the client last set.
    pub fn value(&self, path: &str, index: u32, sub: u32) -> Option<String> {
        self.lock()
            .values
            .get(&(path.to_owned(), index, sub))
            .cloned()
    }

    /// Seed a parameter so a `get` returns it.
    pub fn set_value(&self, path: &str, index: u32, sub: u32, value: impl Into<String>) {
        self.lock()
            .values
            .insert((path.to_owned(), index, sub), value.into());
    }

    /// Push an unsolicited notification to every connected client.
    pub fn notify(&self, line: impl Into<String>) {
        let _ = self.control.send(Some(line.into()));
    }

    /// Drop every connected client, as a console does when it is power-cycled.
    pub fn hangup(&self) {
        let _ = self.control.send(None);
    }

    /// Stop injecting faults, so a reconnected client can succeed.
    pub fn clear_faults(&self) {
        self.lock().faults = Faults::default();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared
            .lock()
            .expect("mock console state is not poisoned")
    }
}

async fn accept_loop(
    listener: TcpListener,
    shared: Arc<Mutex<Shared>>,
    control: broadcast::Sender<Option<String>>,
) {
    loop {
        let Ok((socket, _)) = listener.accept().await else {
            return;
        };
        // Match the client's own no-delay setting; otherwise the server's Nagle would merge the
        // two segments of a deliberately split reply and defeat the fault.
        let _ = socket.set_nodelay(true);

        if let Ok(mut s) = shared.lock() {
            s.connections += 1;
        }

        tokio::spawn(serve(socket, Arc::clone(&shared), control.subscribe()));
    }
}

async fn serve(
    socket: TcpStream,
    shared: Arc<Mutex<Shared>>,
    mut control: broadcast::Receiver<Option<String>>,
) {
    let (mut reader, mut writer) = socket.into_split();
    let mut buf = vec![0u8; 4096];
    let mut pending = Vec::new();

    loop {
        tokio::select! {
            read = reader.read(&mut buf) => {
                let Ok(n) = read else { return };
                if n == 0 {
                    return;
                }
                pending.extend_from_slice(&buf[..n]);

                while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
                    let mut line: Vec<u8> = pending.drain(..=pos).collect();
                    line.pop();
                    let line = String::from_utf8_lossy(&line).trim().to_string();
                    if line.is_empty() {
                        continue;
                    }

                    let (reply, split, hangup) = handle(&shared, &line);

                    if let Some(reply) = reply {
                        if write_reply(&mut writer, &reply, split).await.is_err() {
                            return;
                        }
                    }
                    if hangup {
                        return;
                    }
                }
            }

            msg = control.recv() => {
                match msg {
                    Ok(Some(line)) => {
                        if writer.write_all(format!("{line}\n").as_bytes()).await.is_err() {
                            return;
                        }
                    }
                    // A `None` is an instruction to hang up; a lagged/closed channel ends the
                    // session too, which is the safe direction for a test double.
                    _ => return,
                }
            }
        }
    }
}

/// Write a reply, optionally torn into two TCP segments.
async fn write_reply(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    reply: &str,
    split: bool,
) -> std::io::Result<()> {
    let full = format!("{reply}\n");
    if !split || full.len() < 4 {
        return writer.write_all(full.as_bytes()).await;
    }

    // Tear mid-token, before the newline, so a client that treats one read as one message both
    // loses this reply and is left with a partial line.
    let mid = full.len() / 2;
    writer.write_all(&full.as_bytes()[..mid]).await?;
    tokio::time::sleep(Duration::from_millis(5)).await;
    writer.write_all(&full.as_bytes()[mid..]).await
}

/// Apply one command to the store and decide the reply. Returns (reply, split, hangup).
fn handle(shared: &Arc<Mutex<Shared>>, line: &str) -> (Option<String>, bool, bool) {
    let mut state = shared.lock().expect("mock console state is not poisoned");
    state.received.push(line.to_owned());

    let count = state.received.len();
    let split = state.faults.split_responses;
    let hangup = state.faults.hangup_after.is_some_and(|n| count >= n);

    if state.faults.swallow_replies > 0 {
        state.faults.swallow_replies -= 1;
        return (None, split, hangup);
    }

    let tokens = tokenize(line);
    let verb = tokens.first().map_or("", |(t, _)| t.as_str());

    if let Some((path, _)) = tokens.get(1) {
        if state.faults.error_paths.contains(path) {
            return (Some(format!("ERROR {verb} {path}")), split, hangup);
        }
    }

    let reply = match verb {
        "set" => {
            let path = tokens.get(1).map(|(t, _)| t.clone()).unwrap_or_default();
            let index = tokens.get(2).and_then(|(t, _)| t.parse().ok()).unwrap_or(0);
            let sub = tokens.get(3).and_then(|(t, _)| t.parse().ok()).unwrap_or(0);
            // Store the value with its original quoting so `get` echoes it back faithfully.
            let value = tokens
                .get(4)
                .map(|(t, quoted)| {
                    if *quoted {
                        format!("\"{t}\"")
                    } else {
                        t.clone()
                    }
                })
                .unwrap_or_default();
            state
                .values
                .insert((path.clone(), index, sub), value.clone());
            format!("OK set {path} {index} {sub} {value}")
        }
        "get" => {
            let path = tokens.get(1).map(|(t, _)| t.clone()).unwrap_or_default();
            let index = tokens.get(2).and_then(|(t, _)| t.parse().ok()).unwrap_or(0);
            let sub = tokens.get(3).and_then(|(t, _)| t.parse().ok()).unwrap_or(0);
            let value = state
                .values
                .get(&(path.clone(), index, sub))
                .cloned()
                .unwrap_or_else(|| default_for(&path, index));
            format!("OK get {path} {index} {sub} {value}")
        }
        "ssrecall_ex" | "ssupdate_ex" => format!("OK {line}"),
        "scpmode" => format!("OK {line}"),
        "devstatus" => "OK devstatus runmode \"normal\"".to_owned(),
        "devinfo" => match tokens.get(1).map(|(t, _)| t.as_str()) {
            Some("productname") => "OK devinfo productname \"TF1\"".to_owned(),
            Some("devicename") => "OK devinfo devicename \"MockTF\"".to_owned(),
            other => format!("OK devinfo {} \"\"", other.unwrap_or("")),
        },
        other => format!("ERROR unknown command {other}"),
    };

    (Some(reply), split, hangup)
}

/// What an untouched parameter reads as, matching the defaults in the TF parameter table.
fn default_for(path: &str, index: u32) -> String {
    if path.ends_with("Label/Name") {
        format!("\"ch {}\"", index + 1)
    } else if path.ends_with("Label/Color") {
        "\"Green\"".to_owned()
    } else if path.ends_with("Fader/On") {
        "1".to_owned()
    } else if path.ends_with("Fader/Level") {
        "-32768".to_owned()
    } else {
        "0".to_owned()
    }
}

/// Tokenise, reporting whether each token was quoted so values round-trip with their quoting.
fn tokenize(line: &str) -> Vec<(String, bool)> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut was_quoted = false;
    let mut has_token = false;

    for c in line.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                was_quoted = true;
                has_token = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if has_token {
                    tokens.push((std::mem::take(&mut current), was_quoted));
                    has_token = false;
                    was_quoted = false;
                }
            }
            c => {
                current.push(c);
                has_token = true;
            }
        }
    }
    if has_token {
        tokens.push((current, was_quoted));
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_reports_quoting() {
        let t = tokenize(r#"set X 0 0 "Lead Vox""#);
        assert_eq!(t[4], ("Lead Vox".to_owned(), true));
        assert_eq!(t[2], ("0".to_owned(), false));
    }

    #[tokio::test]
    async fn set_then_get_round_trips_a_quoted_label() {
        let shared = Arc::new(Mutex::new(Shared::default()));
        handle(
            &shared,
            r#"set MIXER:Current/InCh/Label/Name 4 0 "Lead Vox""#,
        );
        let (reply, _, _) = handle(&shared, "get MIXER:Current/InCh/Label/Name 4 0");
        assert_eq!(
            reply.unwrap(),
            r#"OK get MIXER:Current/InCh/Label/Name 4 0 "Lead Vox""#
        );
    }

    #[tokio::test]
    async fn unset_parameters_read_as_their_table_defaults() {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (reply, _, _) = handle(&shared, "get MIXER:Current/InCh/Fader/Level 0 0");
        assert!(reply.unwrap().ends_with("-32768"));
    }
}
