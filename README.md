# TF Bridge

An OSC → Yamaha TF console bridge. It listens for OSC (from QLab) and drives a Yamaha TF mixing
console over Yamaha's Remote Control Protocol (RCP) — faders, curved fades, mutes, sends, labels,
mute groups and scene recalls.

It runs as a small always-on Python service on the theatre's booth Mac, alongside QLab. QLab sends
OSC to `127.0.0.1:8000`; the bridge holds one connection to the console at `192.168.50.2:49280`.

## Layout

- `src/tf-bridge.py` — the service (the deployed bridge).
- `src/selftest.py` — offline unit tests for its pure logic (`python3 selftest.py`).
- `src/docs/OSC-REFERENCE.md` — the full OSC address reference (both grammars).
- `src/docs/tf-bridge-guide.html` — the operator manual + OSC dictionary (a shareable page).
- `src/qlab/` — QLab integration: a copy-paste cue reference, an AppleScript cue generator, and
  an agent-facing automation guide.
- `src/tools/` — small stdlib OSC senders for testing.
- `src/docs/DEPLOY.md` — deploy/rollback runbook for the launchd service on the Mac.
- `src/reference/` — the original `tf3.py` and its launchd plist, for reference/rollback.

## History

The bridge evolved from the venue's original `tf3.py`, keeping its proven blocking-socket networking
and stateful mute/scene logic while adding a richer OSC vocabulary. An earlier Rust implementation
(built on the `yamaha-rcp` crate, upstream at `BrenekH/yamaha-rcp-rs`) was abandoned — a
tokio/long-lived-socket issue made it unreliable on the booth Mac — in favour of the proven Python.

## License

Copyright (c) 2026 Eric Singer. All rights reserved. This is proprietary software with restricted
distribution: it may only be used under an explicit written grant from the owner, which the owner may revoke
at any time. See `LICENSE`. Third-party material (e.g. `src/reference/`, python-osc) is not covered.
