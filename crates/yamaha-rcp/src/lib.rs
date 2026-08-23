/*!
# Yamaha Remote Control Protocol (Rust)

Remote control of [Yamaha mixing consoles](https://usa.yamaha.com/products/proaudio/mixers/index.html)
over TCP/IP.

## Disclaimer

> This library's API is nowhere near the "final" API.
> Development will follow Semantic Versioning, but expect many changes.

> `yamaha-rcp` is mainly tested against the [Yamaha TF Series](https://usa.yamaha.com/products/proaudio/mixers/tf/index.html),
> specifically the TF1. Full support for the Rivage PM, DM7, DM3, CL and QL lines is the goal, but
> the maintainer has no access to those consoles to test against.

## Example

```no_run
use std::net::SocketAddr;
use yamaha_rcp::{Error, Mixer, Strip};

#[tokio::main]
async fn main() -> Result<(), Error> {
    let addr: SocketAddr = "192.168.0.128:49280".parse()?;
    let mixer = Mixer::connect(addr).await?;

    // Set channel 1 (index 0) to -10.00 dB
    mixer.set_fader_db(Strip::Input, 0, -10.0).await?;

    Ok(())
}
```

## Values

Levels are integers in hundredths of a decibel — `-10_00` is -10.00 dB — matching the console's own
representation. [`params::NEG_INF`] (`-32768`) is the console's sentinel for a fader at -∞; it is
not a point on the dB scale, and [`params::ParamInfo::db_to_raw`] maps anything at or below the
minimum onto it. The `f32` accessors ([`Mixer::set_fader_db`], [`Mixer::fader_db`]) convert for you.

## Design notes

* **One connection per console.** Consoles process commands serially, so this crate keeps a single
  socket and paces writes rather than pooling connections.
* **Positional correlation.** RCP has no request IDs; replies arrive in order. Unsolicited
  `NOTIFY` pushes are routed to [`Mixer::notifications`] and never counted as replies.
* **Table-checked commands.** A captured parameter table (see [`params`]) is embedded, so an
  out-of-range channel or a write to a read-only parameter fails locally instead of costing a
  round trip and an `ERROR`.

## Extra documentation

Yamaha publish no usable specification for this protocol. The most complete community reference is
[github.com/BrenekH/yamaha-rcp-docs](https://github.com/BrenekH/yamaha-rcp-docs#readme).
*/

// Decibel literals read far better grouped as `-138_00` than as `-13800`.
#![allow(clippy::inconsistent_digit_grouping)]
#![warn(missing_debug_implementations)]

pub mod connection;
mod error;
pub mod mixer;
pub mod params;
pub mod wire;

pub use connection::{Connection, ConnectionConfig, ConnectionState, Notification, DEFAULT_PORT};
pub use error::Error;
pub use mixer::{Curve, Fade, LabelColor, Mixer, SceneList, Strip};
pub use params::{ParamInfo, ParamTable, MIN_DB_RAW, NEG_INF};
