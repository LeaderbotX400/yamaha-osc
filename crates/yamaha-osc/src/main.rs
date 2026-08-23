//! yamaha-osc — OSC to Yamaha RCP bridge.
//!
//! NOT YET WIRED UP: the modules below are complete and tested, but the CLI that connects
//! them (listener -> router -> dispatcher -> console) is still to be written. The
//! `allow(dead_code)` goes away with it.
#![allow(dead_code)]

mod address;
mod config;
mod discover;
mod dispatch;
mod fade;
mod health;
mod osc;
mod router;
mod value;

fn main() {
    eprintln!("yamaha-osc: the CLI is not implemented yet (see main.rs)");
    std::process::exit(1);
}
