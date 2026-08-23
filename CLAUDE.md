# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`yamaha-rcp` is a Rust library (crate name `yamaha-rcp`, published to crates.io, repo `BrenekH/yamaha-rcp-rs`) for
remote-controlling Yamaha mixing consoles over TCP/IP using Yamaha's Remote Control Protocol (RCP). It is
pre-1.0 and explicitly unstable — the public API is expected to change without much ceremony. It is developed and
tested against the Yamaha TF1; other TF-series/CL/QL/DM/Rivage consoles are untested by the maintainer.

Protocol reference (not in this repo): https://github.com/BrenekH/yamaha-rcp-docs

## Environment: devenv

The Rust toolchain (rustc/cargo/clippy/rustfmt/rust-analyzer, pinned via `languages.rust.channel = "stable"` in
`devenv.nix`, resolved through the `rust-overlay` input in `devenv.yaml`) is managed by
[devenv](https://devenv.sh), not a system-installed Rust. Do not assume a bare `cargo`/`rustc` on `PATH` is this
project's pinned toolchain — always run through devenv:

- One-off command: `devenv shell -- <cmd>` (e.g. `devenv shell -- cargo test`)
- Interactive: `devenv shell`, then run `cargo ...` as usual inside it

If a devenv shell is already active (check `$DEVENV_ROOT` / `$IN_NIX_SHELL`), plain `cargo ...` is fine — don't
re-wrap in another `devenv shell --`.

Changing the toolchain version/components is a `devenv.nix`/`devenv.yaml` edit, not a `rustup` call — this repo has
no `rustup` toolchain file and shouldn't gain one.

## Commands

(run via `devenv shell --`, see above)

- Build: `cargo build`
- Run the example binary (`src/main.rs`) — connects to a real mixer at `192.168.0.128:49280` and drives faders/labels/mutes: `cargo run`
- Test: `cargo test`
- Run a single test: `cargo test <test_name>`
- Lint: `cargo clippy`
- Format: `cargo fmt`

There is no CI config, no docs build step, and no additional dev tooling beyond stock `cargo` (via devenv).

## Architecture

The entire library lives in `src/lib.rs` as a single module; `src/main.rs` is a manual integration example/harness
(not an automated test), useful as a reference for the public API surface but requiring a real console on the
network to actually run.

Core type is `TFMixer`, a cheap-to-`Clone` handle (all shared state is behind `Arc<Mutex<...>>`) around a **pooled
connection** to one mixer:

- `TFMixer::new(addr)` opens one initial TCP connection; further connections are opened lazily up to
  `connection_limit` (default 1, changeable via `set_connection_limit`).
- Each `Connection` pairs a `tokio::net::tcp::OwnedWriteHalf` with an `mpsc::Receiver<String>` fed by a
  dedicated background task that owns the read half. That task reads raw bytes, splits on `\n` (0xA), and forwards
  only lines starting with `"OK"` or `"ERROR"` — this is how request/response correlation is done, since RCP itself
  has no request IDs.
- `send_command` is the single choke point for all I/O: it pops a `Connection` from the pool (creating a new one if
  under the limit, otherwise busy-polling every 10ms for one to free up), writes the command, awaits exactly one
  line off that connection's channel, and pushes the connection back into the pool when done. Because the pool has
  no reservation/ticketing, correctness relies on each `send_command` call fully completing its own
  write-then-single-read before releasing the connection back — do not restructure this into overlapping
  writes/reads on a shared connection.
- `request_bool` / `request_int` / `request_string` are thin parsers layered on `send_command` for the three
  response shapes RCP uses; `request_string` in particular hand-parses quoted, space-containing string values out
  of a space-delimited response line.
- Public API methods (`fader_level`, `set_fader_level`, `muted`, `set_muted`, `color`, `set_color`, `label`,
  `set_label`, `recall_scene`, `fade`) are all just command-string builders on top of the four primitives above.
  Adding a new console parameter follows this same pattern: build the RCP command string, call the appropriate
  `request_*`/`send_command` helper, and expose typed getters/setters.
- `fade` is a client-side ramp: it steps `set_fader_level` on a 50ms tick between two dB values, then snaps to
  `neg_inf_val` if the target was the minimum (i.e. treats `min_fader_val` as "-inf" on the console).

Fader/dB values are represented as scaled integers (hundredths of a dB, e.g. `-10_00` means -10.00 dB) rather than
floats — `min_fader_val`/`max_fader_val`/`neg_inf_val` on `TFMixer` encode the TF1's valid range and its negative-infinity
sentinel. Preserve this integer-scaling convention (including the `_` digit-grouping style, which is why
`clippy::inconsistent_digit_grouping` is allowed) when adding new numeric parameters.

`LabelColor` and `SceneList` are simple enums with hand-written `Display`/`FromStr` (round-tripping through RCP's
string wire format) rather than relying on `serde` for their string representation, even though they derive
`Serialize`/`Deserialize` for structural (de)serialization elsewhere.

All errors funnel through the single `Error` enum (`thiserror`) covering network/parse/protocol failure modes.
