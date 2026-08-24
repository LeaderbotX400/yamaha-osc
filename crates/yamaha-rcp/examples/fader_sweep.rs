//! Fader sweep test across the first 16 input channels. Requires hardware on the network.
//!
//! Drives every channel to two "percent of travel" positions, mapping percent linearly across
//! the console's dB range (100% -> +10 dB max, 50% -> midpoint of MIN_DB_RAW..max), and reads
//! each fader back to confirm the console applied it.
//!
//! ```text
//! cargo run --example fader_sweep -- 192.168.50.2:49280
//! ```

#![allow(clippy::inconsistent_digit_grouping)]

use std::net::SocketAddr;
use std::time::Duration;

use yamaha_rcp::{Mixer, Strip, MIN_DB_RAW};

const CHANNELS: u32 = 16;
const MAX_DB: f32 = 10.0; // console fader ceiling

/// Percent of fader travel -> dB, linear across MIN_DB_RAW..MAX_DB.
fn percent_to_db(pct: f32) -> f32 {
    let min = MIN_DB_RAW as f32 / 100.0; // MIN_DB_RAW is hundredths of a dB
    min + (MAX_DB - min) * (pct / 100.0)
}

async fn sweep(mixer: &Mixer, pct: f32) -> Result<(), Box<dyn std::error::Error>> {
    let target = percent_to_db(pct);
    println!("\n=== {pct:.0}% of travel -> {target:.1} dB on ch1..{CHANNELS} ===");

    for ch in 0..CHANNELS {
        mixer.set_fader_db(Strip::Input, ch, target).await?;
    }
    // Read every channel back and confirm it landed within rounding of the target.
    let mut worst = 0.0f32;
    for ch in 0..CHANNELS {
        let got = mixer.fader_db(Strip::Input, ch).await?;
        let delta = (got - target).abs();
        worst = worst.max(delta);
        let flag = if delta <= 0.05 { "ok" } else { "MISMATCH" };
        println!("  ch{:>2}: {:>7.1} dB  [{}]", ch + 1, got, flag);
    }
    println!("  worst readback delta: {worst:.2} dB");
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let addr: SocketAddr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "192.168.50.2:49280".to_owned())
        .parse()?;

    let mixer = Mixer::connect(addr).await?;
    println!("connected to {addr}");

    sweep(&mixer, 100.0).await?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    sweep(&mixer, 50.0).await?;

    println!("\ndone");
    Ok(())
}
