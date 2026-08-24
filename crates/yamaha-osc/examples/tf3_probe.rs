//! Phase 0: characterize the live `tf3.py` bridge against the real console.
//!
//! Sends legacy `/tf/mute` / `/tf/unmute` OSC at the Python bridge (default the theatre Mac on
//! :8000) and reads the console back over RCP to see exactly which parameters each cue moves.
//! Every mute is immediately restored, so the console and tf3's own desired-state map are left
//! clean. Scene recall is deliberately NOT exercised here (it would load an unknown snapshot).
//!
//! ```text
//! cargo run --example tf3_probe -- 192.168.50.150:8000 192.168.50.2:49280
//! ```

use std::net::SocketAddr;
use std::time::Duration;

use rosc::{encoder, OscMessage, OscPacket, OscType};
use tokio::net::UdpSocket;
use yamaha_rcp::Mixer;

/// How long to wait after an OSC cue for tf3's ~1s reconcile loop to push it to the console.
const SETTLE: Duration = Duration::from_millis(1500);

async fn send_tf(sock: &UdpSocket, target: SocketAddr, addr: &str, arg: &str) {
    let packet = OscPacket::Message(OscMessage {
        addr: addr.to_owned(),
        args: vec![OscType::String(arg.to_owned())],
    });
    let bytes = encoder::encode(&packet).expect("encode OSC");
    sock.send_to(&bytes, target).await.expect("send OSC");
}

/// Read one `.../Fader/On` parameter as 0/1 via a raw RCP get.
async fn read_on(mixer: &Mixer, path: &str, index: u32, sub: u32) -> String {
    let cmd = format!("get {path} {index} {sub}");
    match mixer.raw(cmd).await {
        Ok(reply) => reply
            .split_whitespace()
            .last()
            .unwrap_or("?")
            .to_owned(),
        Err(e) => format!("ERR({e})"),
    }
}

/// A console On-parameter to watch: label + RCP path + index + sub.
struct Probe {
    label: &'static str,
    path: &'static str,
    index: u32,
    sub: u32,
}

async fn snapshot(mixer: &Mixer, probes: &[Probe]) {
    for p in probes {
        let v = read_on(mixer, p.path, p.index, p.sub).await;
        println!("    {:<28} {} {} = {}", p.label, p.path, p.index, v);
    }
}

/// Run one mute→read→unmute→read cycle for a cue, watching the given probes.
async fn cycle(
    sock: &UdpSocket,
    target: SocketAddr,
    mixer: &Mixer,
    arg: &str,
    probes: &[Probe],
) {
    println!("\n=== /tf/mute {arg:?} ===");
    send_tf(sock, target, "/tf/mute", arg).await;
    tokio::time::sleep(SETTLE).await;
    println!("  after mute:");
    snapshot(mixer, probes).await;

    send_tf(sock, target, "/tf/unmute", arg).await;
    tokio::time::sleep(SETTLE).await;
    println!("  after unmute (restore):");
    snapshot(mixer, probes).await;
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let osc_target: SocketAddr = args
        .next()
        .unwrap_or_else(|| "192.168.50.150:8000".to_owned())
        .parse()?;
    let console: SocketAddr = args
        .next()
        .unwrap_or_else(|| "192.168.50.2:49280".to_owned())
        .parse()?;

    let sock = UdpSocket::bind("0.0.0.0:0").await?;
    let mixer = Mixer::connect(console).await?;
    println!("OSC -> tf3 at {osc_target}; reading console at {console}");
    println!("(1 = on/unmuted, 0 = off/muted)");

    // The DCA question: tf3 writes BOTH of these; our Strip::Dca only knows the first.
    let dca = [
        Probe { label: "DCA1 (DCA path)",   path: "MIXER:Current/DCA/Fader/On",   index: 0, sub: 0 },
        Probe { label: "DCA1 (DcaCh path)", path: "MIXER:Current/DcaCh/Fader/On", index: 0, sub: 0 },
    ];
    let input = [Probe { label: "InCh1", path: "MIXER:Current/InCh/Fader/On", index: 0, sub: 0 }];
    let aux = [
        Probe { label: "Aux9 (Mix8)",  path: "MIXER:Current/Mix/Fader/On", index: 8, sub: 0 },
        Probe { label: "Aux10 (Mix9)", path: "MIXER:Current/Mix/Fader/On", index: 9, sub: 0 },
    ];
    let fx = [
        Probe { label: "FX1 a", path: "MIXER:Current/FxRtnCh/Fader/On", index: 0, sub: 0 },
        Probe { label: "FX1 b", path: "MIXER:Current/FxRtnCh/Fader/On", index: 1, sub: 0 },
    ];
    let st = [
        Probe { label: "ST1 a", path: "MIXER:Current/StInCh/Fader/On", index: 0, sub: 0 },
        Probe { label: "ST1 b", path: "MIXER:Current/StInCh/Fader/On", index: 1, sub: 0 },
    ];
    let master = [
        Probe { label: "Master L", path: "MIXER:Current/St/Fader/On", index: 0, sub: 0 },
        Probe { label: "Master R", path: "MIXER:Current/St/Fader/On", index: 1, sub: 0 },
    ];
    let sub = [Probe { label: "Sub (Mono)", path: "MIXER:Current/Mono/Fader/On", index: 0, sub: 0 }];

    println!("\n--- baseline ---");
    for set in [&input[..], &dca[..], &aux[..], &fx[..], &st[..], &master[..], &sub[..]] {
        snapshot(&mixer, set).await;
    }

    cycle(&sock, osc_target, &mixer, "1", &input).await;
    cycle(&sock, osc_target, &mixer, "dca 1", &dca).await;
    cycle(&sock, osc_target, &mixer, "aux 9/10", &aux).await;
    cycle(&sock, osc_target, &mixer, "fx1", &fx).await;
    cycle(&sock, osc_target, &mixer, "st1", &st).await;
    cycle(&sock, osc_target, &mixer, "master", &master).await;
    cycle(&sock, osc_target, &mixer, "sub", &sub).await;

    println!("\ndone — every cue restored");
    Ok(())
}
