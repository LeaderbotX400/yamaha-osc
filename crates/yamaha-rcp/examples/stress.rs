//! Manual smoke test against a real console. Requires hardware on the network.
//!
//! ```text
//! cargo run --example stress -- 192.168.50.20:49280
//! ```

#![allow(clippy::inconsistent_digit_grouping)]

use std::net::SocketAddr;
use std::time::Duration;

use yamaha_rcp::{Curve, Fade, LabelColor, Mixer, Strip};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let addr: SocketAddr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "192.168.0.128:49280".to_owned())
        .parse()?;

    let mixer = Mixer::connect(addr).await?;
    println!("connected to {addr}");

    println!("ch1 label: {}", mixer.label(Strip::Input, 0).await?);
    println!("ch1 colour: {}", mixer.color(Strip::Input, 0).await?);
    println!("ch1 level: {} dB", mixer.fader_db(Strip::Input, 0).await?);
    println!("ch1 on: {}", mixer.on(Strip::Input, 0).await?);

    mixer.set_label(Strip::Input, 1, "CHAN 2").await?;
    mixer.set_color(Strip::Input, 1, LabelColor::Blue).await?;

    // Two simultaneous fades in opposite directions, then back again.
    let (a, b) = tokio::join!(
        mixer.fade(Strip::Input, 1, Fade::to(-40_00, Duration::from_secs(3)).from(10_00)),
        mixer.fade(
            Strip::Input,
            2,
            Fade::to(10_00, Duration::from_secs(3))
                .from(-40_00)
                .curve(Curve::Cosine)
        ),
    );
    a?;
    b?;

    tokio::time::sleep(Duration::from_secs(1)).await;

    // Ramp every input down to -∞ together, which is also a rough throughput test: 16 channels
    // stepping at 33 ms is ~480 commands/second offered against a console that absorbs ~200.
    let mut tasks = Vec::new();
    for ch in 0..16u32 {
        let mixer = mixer.clone();
        tasks.push(tokio::spawn(async move {
            mixer
                .fade(Strip::Input, ch, Fade::to(yamaha_rcp::NEG_INF, Duration::from_secs(3)))
                .await
        }));
    }
    for task in tasks {
        task.await??;
    }

    println!("done");
    Ok(())
}
