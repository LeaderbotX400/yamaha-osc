//! Tiny OSC test client: send one message to a bridge/host.
//!
//! Each trailing argument is parsed as an int, then a float, then falls back to a string.
//!
//! ```text
//! cargo run --example osc_send -- 127.0.0.1:8000 /ch/1/mute 1
//! cargo run --example osc_send -- 127.0.0.1:8000 /ch/1/fader -6.0
//! cargo run --example osc_send -- 127.0.0.1:8000 /scene/recall a 3
//! ```

use std::net::SocketAddr;

use rosc::{encoder, OscMessage, OscPacket, OscType};
use tokio::net::UdpSocket;

fn parse_arg(s: &str) -> OscType {
    if let Ok(i) = s.parse::<i32>() {
        OscType::Int(i)
    } else if let Ok(f) = s.parse::<f32>() {
        OscType::Float(f)
    } else {
        OscType::String(s.to_owned())
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let target: SocketAddr = args.next().expect("target address:port").parse()?;
    let addr = args.next().expect("OSC address");
    let osc_args: Vec<OscType> = args.map(|a| parse_arg(&a)).collect();

    let packet = OscPacket::Message(OscMessage {
        addr: addr.clone(),
        args: osc_args.clone(),
    });
    let bytes = encoder::encode(&packet)?;

    let sock = UdpSocket::bind("0.0.0.0:0").await?;
    sock.send_to(&bytes, target).await?;
    println!("sent {addr} {osc_args:?} -> {target}");
    Ok(())
}
