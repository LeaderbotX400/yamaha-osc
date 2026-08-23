//! Receiving OSC from QLab, over UDP or over TCP with SLIP framing.

use std::net::SocketAddr;
use std::time::Instant;

use anyhow::{Context, Result};
use rosc::{OscMessage, OscPacket};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, trace, warn};

use crate::config::Transport;

/// The largest OSC packet accepted. QLab messages are tens of bytes; this is generous enough for
/// a bundle of them and small enough that a malformed length cannot exhaust memory.
const MAX_PACKET: usize = 64 * 1024;

/// A message as it arrived.
#[derive(Clone, Debug)]
pub struct Incoming {
    pub message: OscMessage,
    pub from: SocketAddr,
    /// Taken the instant the datagram was read, so end-to-end latency can be measured against it.
    pub received: Instant,
}

/// Bind a listener and forward every message it receives.
pub async fn spawn_listener(
    listen: SocketAddr,
    transport: Transport,
    tx: mpsc::Sender<Incoming>,
) -> Result<JoinHandle<()>> {
    Ok(match transport {
        Transport::Udp => {
            let socket = UdpSocket::bind(listen)
                .await
                .with_context(|| format!("binding UDP {listen}"))?;
            debug!(%listen, "listening for OSC over UDP");
            tokio::spawn(udp_loop(socket, tx))
        }
        Transport::Tcp => {
            let listener = TcpListener::bind(listen)
                .await
                .with_context(|| format!("binding TCP {listen}"))?;
            debug!(%listen, "listening for OSC over TCP (SLIP framed)");
            tokio::spawn(tcp_loop(listener, tx))
        }
    })
}

async fn udp_loop(socket: UdpSocket, tx: mpsc::Sender<Incoming>) {
    let mut buf = vec![0u8; MAX_PACKET];
    loop {
        let (n, from) = match socket.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "OSC receive failed");
                continue;
            }
        };
        let received = Instant::now();
        if !deliver(&buf[..n], from, received, &tx).await {
            return;
        }
    }
}

async fn tcp_loop(listener: TcpListener, tx: mpsc::Sender<Incoming>) {
    loop {
        let Ok((socket, from)) = listener.accept().await else {
            return;
        };
        debug!(%from, "OSC client connected");
        tokio::spawn(tcp_session(socket, from, tx.clone()));
    }
}

async fn tcp_session(
    mut socket: tokio::net::TcpStream,
    from: SocketAddr,
    tx: mpsc::Sender<Incoming>,
) {
    let _ = socket.set_nodelay(true);
    let mut decoder = SlipDecoder::new();
    let mut buf = vec![0u8; 8192];

    loop {
        let n = match socket.read(&mut buf).await {
            Ok(0) => {
                debug!(%from, "OSC client disconnected");
                return;
            }
            Ok(n) => n,
            Err(e) => {
                warn!(%from, error = %e, "OSC read failed");
                return;
            }
        };
        let received = Instant::now();

        for packet in decoder.push(&buf[..n]) {
            if !deliver(&packet, from, received, &tx).await {
                return;
            }
        }
    }
}

/// Decode one packet and forward the messages in it. Returns false if the consumer has gone away.
async fn deliver(
    bytes: &[u8],
    from: SocketAddr,
    received: Instant,
    tx: &mpsc::Sender<Incoming>,
) -> bool {
    let packet = match rosc::decoder::decode_udp(bytes) {
        Ok((_, packet)) => packet,
        Err(e) => {
            warn!(%from, error = ?e, len = bytes.len(), "ignoring malformed OSC packet");
            return true;
        }
    };

    let mut messages = Vec::new();
    flatten(packet, &mut messages);

    for message in messages {
        trace!(%from, address = %message.addr, "OSC in");
        let incoming = Incoming {
            message,
            from,
            received,
        };
        // A full queue means the console is already saturated. Dropping here would silently lose a
        // cue, so this waits — the queue is deep enough that reaching this point is pathological.
        if tx.send(incoming).await.is_err() {
            return false;
        }
    }
    true
}

/// Flatten a packet into plain messages.
///
/// Bundle time tags are ignored: QLab has already decided when a cue fires, and holding a message
/// back to honour a tag would add latency to the one thing that must not have any.
pub fn flatten(packet: OscPacket, out: &mut Vec<OscMessage>) {
    match packet {
        OscPacket::Message(m) => out.push(m),
        OscPacket::Bundle(b) => {
            for inner in b.content {
                flatten(inner, out);
            }
        }
    }
}

// ---- SLIP (RFC 1055) --------------------------------------------------------------------------

const END: u8 = 0xC0;
const ESC: u8 = 0xDB;
const ESC_END: u8 = 0xDC;
const ESC_ESC: u8 = 0xDD;

/// Incremental SLIP decoder for OSC-over-TCP.
///
/// OSC 1.1 — and QLab — frame TCP packets with "double END" SLIP: an `END` byte both before and
/// after each packet. A leading `END` therefore just closes an empty frame, which is why empty
/// frames are skipped rather than reported.
#[derive(Debug, Default)]
pub struct SlipDecoder {
    buf: Vec<u8>,
    escaped: bool,
    /// Set when a frame overruns; bytes are discarded until the next frame boundary.
    overrun: bool,
}

impl SlipDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed received bytes and return every complete packet they finish.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut packets = Vec::new();

        for &b in bytes {
            match b {
                END => {
                    if self.overrun {
                        warn!("discarding oversized SLIP frame");
                        self.overrun = false;
                    } else if !self.buf.is_empty() {
                        packets.push(std::mem::take(&mut self.buf));
                    }
                    self.buf.clear();
                    self.escaped = false;
                }
                ESC => self.escaped = true,
                b => {
                    let byte = match (self.escaped, b) {
                        (true, ESC_END) => END,
                        (true, ESC_ESC) => ESC,
                        // An invalid escape sequence: RFC 1055 says to treat the byte as itself.
                        (true, other) => other,
                        (false, other) => other,
                    };
                    self.escaped = false;
                    if self.buf.len() >= MAX_PACKET {
                        self.overrun = true;
                        self.buf.clear();
                    } else if !self.overrun {
                        self.buf.push(byte);
                    }
                }
            }
        }

        packets
    }
}

/// Frame a packet for sending over SLIP, with the leading and trailing `END` OSC 1.1 expects.
pub fn slip_encode(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 4);
    out.push(END);
    for &b in payload {
        match b {
            END => out.extend_from_slice(&[ESC, ESC_END]),
            ESC => out.extend_from_slice(&[ESC, ESC_ESC]),
            b => out.push(b),
        }
    }
    out.push(END);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rosc::{OscBundle, OscTime, OscType};

    fn message(addr: &str) -> OscMessage {
        OscMessage {
            addr: addr.to_owned(),
            args: vec![OscType::Int(1)],
        }
    }

    #[test]
    fn slip_round_trips_a_payload() {
        let payload = b"/ch/1/fader\0\0,f\0\0";
        let mut decoder = SlipDecoder::new();
        assert_eq!(decoder.push(&slip_encode(payload)), [payload.to_vec()]);
    }

    /// The bytes SLIP uses as delimiters can appear inside an OSC payload, so they must survive
    /// escaping intact — otherwise a fader value happening to contain 0xC0 truncates the packet.
    #[test]
    fn delimiter_bytes_inside_a_payload_survive_escaping() {
        let payload = vec![0x01, END, 0x02, ESC, 0x03, END, ESC];
        let encoded = slip_encode(&payload);
        assert!(
            encoded[1..encoded.len() - 1].iter().all(|&b| b != END),
            "no raw END may remain inside the frame"
        );

        let mut decoder = SlipDecoder::new();
        assert_eq!(decoder.push(&encoded), [payload]);
    }

    #[test]
    fn a_frame_split_across_reads_is_reassembled() {
        let payload = b"/ch/1/fader";
        let encoded = slip_encode(payload);
        let (a, b) = encoded.split_at(5);

        let mut decoder = SlipDecoder::new();
        assert!(decoder.push(a).is_empty());
        assert_eq!(decoder.push(b), [payload.to_vec()]);
    }

    #[test]
    fn several_frames_in_one_read_all_come_out() {
        let mut bytes = slip_encode(b"one");
        bytes.extend(slip_encode(b"two"));
        bytes.extend(slip_encode(b"three"));

        let mut decoder = SlipDecoder::new();
        assert_eq!(
            decoder.push(&bytes),
            [b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]
        );
    }

    /// Double-END framing means an empty frame between packets is normal, not an error.
    #[test]
    fn empty_frames_are_skipped() {
        let mut decoder = SlipDecoder::new();
        assert!(decoder.push(&[END, END, END]).is_empty());
        assert_eq!(decoder.push(&[END, b'x', END]), [b"x".to_vec()]);
    }

    #[test]
    fn an_oversized_frame_is_discarded_without_growing_forever() {
        let mut decoder = SlipDecoder::new();
        decoder.push(&[END]);
        decoder.push(&vec![b'x'; MAX_PACKET + 100]);
        assert!(decoder.buf.len() <= MAX_PACKET);

        // The frame is dropped, and the decoder recovers for the next one.
        assert!(decoder.push(&[END]).is_empty());
        assert_eq!(decoder.push(&[b'y', END]), [b"y".to_vec()]);
    }

    #[test]
    fn bundles_are_flattened_into_their_messages() {
        let bundle = OscPacket::Bundle(OscBundle {
            timetag: OscTime::from((0, 0)),
            content: vec![
                OscPacket::Message(message("/ch/1/fader")),
                OscPacket::Bundle(OscBundle {
                    timetag: OscTime::from((0, 0)),
                    content: vec![OscPacket::Message(message("/ch/2/fader"))],
                }),
            ],
        });

        let mut out = Vec::new();
        flatten(bundle, &mut out);
        assert_eq!(
            out.iter().map(|m| m.addr.as_str()).collect::<Vec<_>>(),
            ["/ch/1/fader", "/ch/2/fader"]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_udp_message_reaches_the_consumer() {
        let (tx, mut rx) = mpsc::channel(16);
        let listen: SocketAddr = "127.0.0.1:0".parse().unwrap();

        // Bind explicitly so the test knows the port the listener chose.
        let socket = UdpSocket::bind(listen).await.unwrap();
        let addr = socket.local_addr().unwrap();
        tokio::spawn(udp_loop(socket, tx));

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let packet = rosc::encoder::encode(&OscPacket::Message(message("/ch/1/fader"))).unwrap();
        client.send_to(&packet, addr).await.unwrap();

        let incoming = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("should not time out")
            .expect("should receive a message");
        assert_eq!(incoming.message.addr, "/ch/1/fader");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_malformed_packet_is_ignored_without_killing_the_listener() {
        let (tx, mut rx) = mpsc::channel(16);
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        tokio::spawn(udp_loop(socket, tx));

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.send_to(b"this is not OSC", addr).await.unwrap();

        // A good message straight after must still arrive.
        let packet = rosc::encoder::encode(&OscPacket::Message(message("/ch/2/fader"))).unwrap();
        client.send_to(&packet, addr).await.unwrap();

        let incoming = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("should not time out")
            .expect("should receive a message");
        assert_eq!(incoming.message.addr, "/ch/2/fader");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_slip_framed_tcp_message_reaches_the_consumer() {
        use tokio::io::AsyncWriteExt;

        let (tx, mut rx) = mpsc::channel(16);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(tcp_loop(listener, tx));

        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let packet = rosc::encoder::encode(&OscPacket::Message(message("/ch/3/fader"))).unwrap();
        client.write_all(&slip_encode(&packet)).await.unwrap();

        let incoming = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("should not time out")
            .expect("should receive a message");
        assert_eq!(incoming.message.addr, "/ch/3/fader");
    }
}
