//! Reporting console connection state back out over OSC.
//!
//! One-way control has an obvious failure mode: if the console link is down, cues fire into
//! nothing and the first anyone knows is the silence. Pointing this at QLab's OSC input makes the
//! link's state visible in the workspace, so a cue can check it before the house is in.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, Result};
use rosc::{OscMessage, OscPacket, OscType};
use tokio::net::UdpSocket;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use yamaha_rcp::ConnectionState;

/// Address the bridge reports its console link state on.
pub const STATUS_ADDRESS: &str = "/bridge/status";

/// Send `/bridge/status` whenever the console link changes, and optionally on a heartbeat.
pub async fn spawn(
    destination: SocketAddr,
    mut state: watch::Receiver<ConnectionState>,
    heartbeat: Duration,
) -> Result<JoinHandle<()>> {
    // Bind an ephemeral port on the same address family as the destination.
    let bind = if destination.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let socket = UdpSocket::bind(bind)
        .await
        .with_context(|| format!("binding a UDP socket to report health to {destination}"))?;

    debug!(%destination, ?heartbeat, "reporting bridge health over OSC");

    Ok(tokio::spawn(async move {
        // Report the starting state immediately, so a listener that was already running is not
        // left guessing until something changes. The watch guard is dropped before the await:
        // holding one across a suspension point would make this future non-Send and could block
        // the connection actor from publishing its next state.
        let initial = state.borrow().clone();
        send(&socket, destination, &initial).await;

        loop {
            let changed = state.changed();
            let current = if heartbeat.is_zero() {
                if changed.await.is_err() {
                    return;
                }
                state.borrow().clone()
            } else {
                match tokio::time::timeout(heartbeat, changed).await {
                    // State changed before the heartbeat was due.
                    Ok(Ok(())) => state.borrow().clone(),
                    // The bridge is shutting down.
                    Ok(Err(_)) => return,
                    // Heartbeat: repeat whatever the state currently is.
                    Err(_) => state.borrow().clone(),
                }
            };
            send(&socket, destination, &current).await;
        }
    }))
}

/// The OSC arguments for a state: a machine-readable word, then a human-readable detail.
fn args_for(state: &ConnectionState) -> Vec<OscType> {
    match state {
        ConnectionState::Connected => vec![
            OscType::String("connected".into()),
            OscType::String(String::new()),
        ],
        ConnectionState::Connecting => vec![
            OscType::String("connecting".into()),
            OscType::String(String::new()),
        ],
        ConnectionState::Disconnected(reason) => vec![
            OscType::String("disconnected".into()),
            OscType::String(reason.clone()),
        ],
    }
}

async fn send(socket: &UdpSocket, destination: SocketAddr, state: &ConnectionState) {
    let packet = OscPacket::Message(OscMessage {
        addr: STATUS_ADDRESS.to_owned(),
        args: args_for(state),
    });

    let Ok(bytes) = rosc::encoder::encode(&packet) else {
        warn!("could not encode a health message");
        return;
    };
    if let Err(e) = socket.send_to(&bytes, destination).await {
        // Health reporting must never be able to take the bridge down with it.
        warn!(%destination, error = %e, "could not send health message");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(state: &ConnectionState) -> String {
        match &args_for(state)[0] {
            OscType::String(s) => s.clone(),
            other => panic!("expected a string, got {other:?}"),
        }
    }

    #[test]
    fn every_state_maps_to_a_machine_readable_word() {
        assert_eq!(word(&ConnectionState::Connected), "connected");
        assert_eq!(word(&ConnectionState::Connecting), "connecting");
        assert_eq!(
            word(&ConnectionState::Disconnected("boom".into())),
            "disconnected"
        );
    }

    #[test]
    fn the_reason_is_carried_as_a_second_argument() {
        let args = args_for(&ConnectionState::Disconnected("console closed".into()));
        assert_eq!(args.len(), 2);
        assert_eq!(args[1], OscType::String("console closed".into()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_current_state_is_reported_without_waiting_for_a_change() {
        let listener = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let destination = listener.local_addr().unwrap();
        let (_tx, rx) = watch::channel(ConnectionState::Connected);

        spawn(destination, rx, Duration::ZERO).await.unwrap();

        let mut buf = vec![0u8; 1024];
        let (n, _) = tokio::time::timeout(Duration::from_secs(2), listener.recv_from(&mut buf))
            .await
            .expect("a status message should arrive promptly")
            .unwrap();

        let (_, packet) = rosc::decoder::decode_udp(&buf[..n]).unwrap();
        let OscPacket::Message(message) = packet else {
            panic!("expected a message");
        };
        assert_eq!(message.addr, STATUS_ADDRESS);
        assert_eq!(message.args[0], OscType::String("connected".into()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_state_change_is_reported() {
        let listener = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let destination = listener.local_addr().unwrap();
        let (tx, rx) = watch::channel(ConnectionState::Connected);

        spawn(destination, rx, Duration::ZERO).await.unwrap();

        let mut buf = vec![0u8; 1024];
        // The initial "connected" report.
        tokio::time::timeout(Duration::from_secs(2), listener.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();

        tx.send(ConnectionState::Disconnected("unplugged".into()))
            .unwrap();

        let (n, _) = tokio::time::timeout(Duration::from_secs(2), listener.recv_from(&mut buf))
            .await
            .expect("the change should be reported")
            .unwrap();
        let (_, packet) = rosc::decoder::decode_udp(&buf[..n]).unwrap();
        let OscPacket::Message(message) = packet else {
            panic!("expected a message");
        };
        assert_eq!(message.args[0], OscType::String("disconnected".into()));
        assert_eq!(message.args[1], OscType::String("unplugged".into()));
    }
}
