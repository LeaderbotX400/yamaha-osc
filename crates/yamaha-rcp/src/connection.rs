//! The connection actor: one owned TCP socket to one console.
//!
//! # Why one connection and not a pool
//!
//! A mixing console is a single serial device. The previous implementation kept a `Vec` of
//! connections and, when none was free, busy-polled it every 10 ms — putting a 10 ms floor under
//! any command that arrived while another was in flight, which is the exact opposite of what a cue
//! path needs. Extra sockets do not make the console process commands faster, so this owns exactly
//! one and paces writes to a rate the console is known to keep up with.
//!
//! # Correlation
//!
//! RCP has no request IDs. Replies arrive in the order the commands were sent, so in-flight
//! commands sit in a FIFO and each response completes the one at the front. This only holds if
//! unsolicited `NOTIFY` lines are excluded from the reply stream — the console pushes those at any
//! time, and counting one as a reply would misalign every subsequent command with its response.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::time::{Instant, MissedTickBehavior};
use tracing::{debug, trace, warn};

use crate::wire::{tokenize, LineBuffer, Status};
use crate::Error;

/// Default RCP port on every Yamaha console that speaks it.
pub const DEFAULT_PORT: u16 = 49280;

/// Tuning for a console connection. Defaults are chosen for a wired LAN and a live show.
#[derive(Clone, Debug)]
pub struct ConnectionConfig {
    /// How long to wait for the TCP handshake.
    pub connect_timeout: Duration,
    /// How long to wait for a reply before giving up on a command.
    pub response_timeout: Duration,
    /// Minimum spacing between writes.
    ///
    /// 5 ms (200 commands/second) is the pacing the Bitfocus Companion module has used against
    /// these consoles for years; it is the one number here with real field mileage behind it.
    pub min_command_interval: Duration,
    /// How often to prod the console so a silently dead link is noticed.
    pub keepalive_interval: Duration,
    /// First reconnect delay after a drop.
    pub reconnect_min_backoff: Duration,
    /// Ceiling for the reconnect delay.
    pub reconnect_max_backoff: Duration,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(3),
            response_timeout: Duration::from_millis(1000),
            min_command_interval: Duration::from_millis(5),
            keepalive_interval: Duration::from_secs(10),
            reconnect_min_backoff: Duration::from_millis(100),
            reconnect_max_backoff: Duration::from_secs(5),
        }
    }
}

/// Whether the console is currently reachable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    /// A connection attempt is in progress.
    Connecting,
    /// Connected; commands will be sent.
    Connected,
    /// Not connected. Commands fail immediately with [`Error::Disconnected`].
    Disconnected(String),
}

impl ConnectionState {
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected)
    }
}

/// An unsolicited change notification pushed by the console.
///
/// The console sends these to every connected client whenever a parameter changes — from the
/// physical surface, from StageMix, or from another RCP client. No subscription is required.
#[derive(Clone, Debug)]
pub struct Notification {
    /// The full line as received, minus the terminator.
    pub raw: String,
    /// Tokenised form, starting at the verb (the leading `NOTIFY` is stripped).
    pub tokens: Vec<String>,
}

impl Notification {
    /// The verb, e.g. `set` or `sscurrent_ex`.
    pub fn verb(&self) -> &str {
        self.tokens.first().map_or("", String::as_str)
    }

    /// The parameter path, for `NOTIFY set` lines.
    pub fn path(&self) -> Option<&str> {
        (self.verb() == "set").then(|| self.tokens.get(1).map(String::as_str))?
    }
}

/// A command queued for the console.
struct Request {
    line: String,
    /// `None` for fire-and-forget: the reply is still correlated and logged, but no caller waits.
    reply: Option<oneshot::Sender<Result<String, Error>>>,
}

/// A command that has been written and is awaiting its reply.
struct Pending {
    line: String,
    deadline: Instant,
    reply: Option<oneshot::Sender<Result<String, Error>>>,
}

impl Pending {
    /// Deliver an outcome, ignoring a caller that has since given up.
    fn complete(self, outcome: Result<String, Error>) {
        match (&outcome, self.reply) {
            (_, Some(tx)) => {
                let _ = tx.send(outcome);
            }
            // Fire-and-forget: nobody is waiting, so a failure would otherwise vanish silently.
            (Err(e), None) => warn!(command = %self.line, error = %e, "command failed"),
            (Ok(_), None) => {}
        }
    }
}

/// Handle to a connection actor. Cheap to clone; all clones drive the same socket.
#[derive(Clone, Debug)]
pub struct Connection {
    tx: mpsc::Sender<Request>,
    state: watch::Receiver<ConnectionState>,
    notifications: Arc<broadcast::Sender<Notification>>,
    response_timeout: Duration,
}

impl Connection {
    /// Spawn a connection actor for `addr` and wait for the first connection attempt to settle.
    ///
    /// Returns an error only if the *initial* connection fails; after that the actor reconnects on
    /// its own and callers observe outages through [`Connection::state`].
    pub async fn connect(addr: SocketAddr, config: ConnectionConfig) -> Result<Self, Error> {
        let (tx, rx) = mpsc::channel(256);
        let (state_tx, state_rx) = watch::channel(ConnectionState::Connecting);
        let (notify_tx, _) = broadcast::channel(256);
        let notify_tx = Arc::new(notify_tx);

        // Connect once up front so a typo in the address is reported to the caller rather than
        // disappearing into a reconnect loop.
        let stream = connect_stream(addr, config.connect_timeout).await?;
        let _ = state_tx.send(ConnectionState::Connected);

        let response_timeout = config.response_timeout;
        let actor = Actor {
            addr,
            config,
            rx,
            state: state_tx,
            notifications: Arc::clone(&notify_tx),
        };
        tokio::spawn(actor.run(Some(stream)));

        Ok(Self {
            tx,
            state: state_rx,
            notifications: notify_tx,
            response_timeout,
        })
    }

    /// Watch the connection state.
    pub fn state(&self) -> watch::Receiver<ConnectionState> {
        self.state.clone()
    }

    /// Whether the console is reachable right now.
    pub fn is_connected(&self) -> bool {
        self.state.borrow().is_connected()
    }

    /// Subscribe to unsolicited console notifications.
    pub fn notifications(&self) -> broadcast::Receiver<Notification> {
        self.notifications.subscribe()
    }

    /// Send a command and wait for the console's reply.
    pub async fn request(&self, line: impl Into<String>) -> Result<String, Error> {
        let line = line.into();
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Request {
                line: line.clone(),
                reply: Some(reply_tx),
            })
            .await
            .map_err(|_| Error::Disconnected)?;

        // The actor applies its own per-command deadline; this outer guard only covers the case
        // where the actor itself dies without draining its queue.
        match tokio::time::timeout(self.response_timeout * 2, reply_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(Error::Disconnected),
            Err(_) => Err(Error::Timeout {
                command: line,
                timeout_ms: self.response_timeout.as_millis() as u64 * 2,
            }),
        }
    }

    /// Queue a command without waiting for its reply.
    ///
    /// This is the cue-path send: it returns as soon as the command is queued, taking a full
    /// network round trip out of the critical path. The reply is still correlated, and a console
    /// `ERROR` is logged rather than returned.
    pub async fn send(&self, line: impl Into<String>) -> Result<(), Error> {
        self.tx
            .send(Request {
                line: line.into(),
                reply: None,
            })
            .await
            .map_err(|_| Error::Disconnected)
    }
}

/// Open a TCP connection with a timeout, without blocking the runtime.
///
/// The previous implementation called the blocking `std::net::TcpStream::connect_timeout` from
/// async context, stalling a runtime worker thread for up to the full timeout.
async fn connect_stream(addr: SocketAddr, timeout: Duration) -> Result<TcpStream, Error> {
    let stream = tokio::time::timeout(timeout, TcpStream::connect(addr))
        .await
        .map_err(|_| {
            Error::Network(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("connecting to {addr} timed out after {timeout:?}"),
            ))
        })??;

    // Without this, Nagle's algorithm holds a small command back waiting for more data to
    // coalesce with, and interacts with the peer's delayed ACK to add tens of milliseconds to a
    // cue. Every command this crate sends is small, and every one of them is latency-sensitive.
    stream.set_nodelay(true)?;

    Ok(stream)
}

struct Actor {
    addr: SocketAddr,
    config: ConnectionConfig,
    rx: mpsc::Receiver<Request>,
    state: watch::Sender<ConnectionState>,
    notifications: Arc<broadcast::Sender<Notification>>,
}

impl Actor {
    /// Own the socket for as long as the handle lives, reconnecting after drops.
    async fn run(mut self, initial: Option<TcpStream>) {
        let mut stream = initial;
        let mut backoff = self.config.reconnect_min_backoff;

        loop {
            let socket = match stream.take() {
                Some(s) => s,
                None => {
                    let _ = self.state.send(ConnectionState::Connecting);
                    match connect_stream(self.addr, self.config.connect_timeout).await {
                        Ok(s) => {
                            debug!(addr = %self.addr, "reconnected to console");
                            backoff = self.config.reconnect_min_backoff;
                            let _ = self.state.send(ConnectionState::Connected);
                            s
                        }
                        Err(e) => {
                            let _ = self.state.send(ConnectionState::Disconnected(e.to_string()));
                            warn!(addr = %self.addr, error = %e, retry_in = ?backoff,
                                  "console connect failed");
                            tokio::time::sleep(jittered(backoff)).await;
                            backoff =
                                (backoff * 2).min(self.config.reconnect_max_backoff);
                            continue;
                        }
                    }
                }
            };

            match self.serve(socket).await {
                Session::HandleDropped => {
                    debug!("all connection handles dropped; actor stopping");
                    return;
                }
                Session::Lost(reason) => {
                    let _ = self
                        .state
                        .send(ConnectionState::Disconnected(reason.clone()));
                    warn!(addr = %self.addr, %reason, retry_in = ?backoff, "console link lost");
                    tokio::time::sleep(jittered(backoff)).await;
                    backoff = (backoff * 2).min(self.config.reconnect_max_backoff);
                }
            }
        }
    }

    /// Drive one connected socket until it fails or the last handle is dropped.
    async fn serve(&mut self, socket: TcpStream) -> Session {
        let (mut reader, mut writer) = socket.into_split();
        let mut lines = LineBuffer::new();
        let mut inflight: VecDeque<Pending> = VecDeque::new();
        let mut read_buf = vec![0u8; 4096];
        let mut last_write = Instant::now() - self.config.min_command_interval;

        // The console holds a link open only while it sees traffic; `scpmode keepalive` tells it
        // how often to expect us, and the periodic `devstatus runmode` is that traffic.
        if writer
            .write_all(
                format!(
                    "scpmode keepalive {}\n",
                    self.config.keepalive_interval.as_millis()
                )
                .as_bytes(),
            )
            .await
            .is_err()
        {
            return Session::Lost("keepalive handshake failed".into());
        }
        inflight.push_back(Pending {
            line: "scpmode keepalive".into(),
            deadline: Instant::now() + self.config.response_timeout,
            reply: None,
        });

        let mut keepalive = tokio::time::interval(self.config.keepalive_interval);
        keepalive.set_missed_tick_behavior(MissedTickBehavior::Delay);
        keepalive.tick().await; // the first tick is immediate; we just sent the handshake

        loop {
            // Only wait on a deadline if something is actually in flight.
            let deadline = inflight.front().map(|p| p.deadline);
            // Respect the pacing floor: if we wrote recently, do not accept a new command yet.
            let write_ready_at = last_write + self.config.min_command_interval;
            let can_write = Instant::now() >= write_ready_at;

            tokio::select! {
                biased;

                // Reading first: draining replies keeps the in-flight queue short and is what
                // unblocks callers, so it should never be starved by a busy command stream.
                read = reader.read(&mut read_buf) => {
                    let n = match read {
                        Ok(0) => return self.fail_all(inflight, "console closed the connection"),
                        Ok(n) => n,
                        Err(e) => return self.fail_all(inflight, &format!("read failed: {e}")),
                    };
                    let received = match lines.push(&read_buf[..n]) {
                        Ok(l) => l,
                        Err(desync) => {
                            return self.fail_all(inflight, &format!("protocol desync: {desync}"));
                        }
                    };
                    for line in received {
                        self.handle_line(line, &mut inflight);
                    }
                }

                // A command timed out. RCP replies are ordered, so a missing reply would stall
                // every command behind it; failing the head keeps the queue moving.
                _ = sleep_until_opt(deadline), if deadline.is_some() => {
                    if let Some(p) = inflight.pop_front() {
                        let timeout_ms = self.config.response_timeout.as_millis() as u64;
                        warn!(command = %p.line, timeout_ms, "no reply from console");
                        let line = p.line.clone();
                        p.complete(Err(Error::Timeout { command: line, timeout_ms }));
                    }
                }

                _ = keepalive.tick() => {
                    if writer.write_all(b"devstatus runmode\n").await.is_err() {
                        return self.fail_all(inflight, "keepalive write failed");
                    }
                    last_write = Instant::now();
                    inflight.push_back(Pending {
                        line: "devstatus runmode".into(),
                        deadline: Instant::now() + self.config.response_timeout,
                        reply: None,
                    });
                }

                // Pacing gate: when a write happened within min_command_interval, this branch is
                // disabled and the timer branch below re-enables it, so commands queue in the
                // channel instead of flooding the console.
                request = self.rx.recv(), if can_write => {
                    let Some(request) = request else {
                        return Session::HandleDropped;
                    };
                    trace!(command = %request.line, "-> console");
                    if let Err(e) = writer.write_all(format!("{}\n", request.line).as_bytes()).await {
                        // The command never reached the console, so fail it explicitly rather
                        // than leaking it: the old code returned early here and dropped the
                        // connection on the floor, starving the pool permanently.
                        let reason = format!("write failed: {e}");
                        request.reply.map(|tx| tx.send(Err(Error::Network(
                            std::io::Error::other(reason.clone())))));
                        return self.fail_all(inflight, &reason);
                    }
                    last_write = Instant::now();
                    inflight.push_back(Pending {
                        line: request.line,
                        deadline: Instant::now() + self.config.response_timeout,
                        reply: request.reply,
                    });
                }

                // Re-arm the loop when the pacing window expires so a queued command goes out
                // promptly instead of waiting for unrelated activity.
                _ = tokio::time::sleep_until(write_ready_at), if !can_write => {}
            }
        }
    }

    /// Route one line from the console: a reply completes the oldest in-flight command, a
    /// notification goes to subscribers.
    fn handle_line(&self, line: String, inflight: &mut VecDeque<Pending>) {
        let tokens = tokenize(&line);
        let Some(status) = tokens.first().and_then(|t| Status::from_token(t)) else {
            trace!(%line, "ignoring unrecognised console line");
            return;
        };

        if status == Status::Notify {
            let _ = self.notifications.send(Notification {
                raw: line,
                tokens: tokens.into_iter().skip(1).collect(),
            });
            return;
        }

        let Some(pending) = inflight.pop_front() else {
            trace!(%line, "reply with no command outstanding");
            return;
        };

        trace!(%line, "<- console");
        let outcome = match status {
            Status::Error => Err(Error::Console(line)),
            _ => Ok(line),
        };
        pending.complete(outcome);
    }

    /// Fail every outstanding command so no caller waits on a socket that is gone.
    fn fail_all(&self, inflight: VecDeque<Pending>, reason: &str) -> Session {
        for pending in inflight {
            pending.complete(Err(Error::Network(std::io::Error::other(
                reason.to_owned(),
            ))));
        }
        Session::Lost(reason.to_owned())
    }
}

/// Why a session ended.
enum Session {
    /// The socket failed; reconnect.
    Lost(String),
    /// Every [`Connection`] handle was dropped; stop.
    HandleDropped,
}

/// `sleep_until` for an optional deadline. Guarded by a `select!` condition, so the `None` case is
/// never actually polled.
async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

/// Spread reconnect attempts over ±25% so several bridges restarting together do not synchronise
/// into a thundering herd against one console.
fn jittered(d: Duration) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |t| t.subsec_nanos() as u64);
    let spread = d.as_millis() as u64 / 2;
    if spread == 0 {
        return d;
    }
    let offset = nanos % spread;
    d - Duration::from_millis(spread / 2) + Duration::from_millis(offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_stays_within_the_original_order_of_magnitude() {
        for ms in [100u64, 500, 5000] {
            let d = Duration::from_millis(ms);
            let j = jittered(d);
            assert!(j >= d / 2 && j <= d * 2, "{ms}ms jittered to {j:?}");
        }
    }

    #[test]
    fn jitter_is_a_noop_for_tiny_durations() {
        let d = Duration::from_millis(1);
        assert_eq!(jittered(d), d);
    }

    #[test]
    fn notification_parses_a_set_push() {
        let n = Notification {
            raw: "NOTIFY set MIXER:Current/InCh/Fader/On 4 0 0".into(),
            tokens: tokenize("set MIXER:Current/InCh/Fader/On 4 0 0"),
        };
        assert_eq!(n.verb(), "set");
        assert_eq!(n.path(), Some("MIXER:Current/InCh/Fader/On"));
    }

    #[test]
    fn notification_path_is_none_for_scene_pushes() {
        let n = Notification {
            raw: "NOTIFY sscurrent_ex scene_a 12".into(),
            tokens: tokenize("sscurrent_ex scene_a 12"),
        };
        assert_eq!(n.verb(), "sscurrent_ex");
        assert_eq!(n.path(), None);
    }
}
