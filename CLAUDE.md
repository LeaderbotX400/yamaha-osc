# CLAUDE.md

Guidance for Claude Code (claude.ai/code) working in this repository.

## What this is

An **OSC → Yamaha TF console bridge**. `bridge/tf-bridge.py` is a small Python service that listens
for OSC (from QLab) and drives a Yamaha TF mixing console over Yamaha's Remote Control Protocol (RCP,
newline-terminated ASCII over TCP). It runs on the theatre's booth Mac beside QLab, translating cue
messages into console moves: faders in dB, curved fades, mutes, sends, labels, mute groups, scenes.

Developed and run against a **Yamaha TF3** at `192.168.50.2:49280`. The bridge listens on UDP
`0.0.0.0:8000`; QLab reaches it on loopback (`127.0.0.1:8000`).

## Where it runs

The service is deployed to the booth Mac (`theatretechs@192.168.50.x`) and kept alive by a user
launchd agent `tf3.server` (`~/Library/LaunchAgents/tf3.server.plist`), which runs `~/Scripts/tf3.py`
(that filename now holds this bridge; the venue's original is preserved as `~/Scripts/tf3.py.orig`).
Interpreter: `/Library/Frameworks/Python.framework/Versions/3.14/bin/python3` (it has `python-osc`;
the bare `python3` in an ssh shell does not). Logs: `/tmp/tf3.out.log`, `/tmp/tf3.err.log`.
Deploy/rollback runbook: `bridge/DEPLOY.md`.

## Commands

- Offline logic tests (no console, no `python-osc` needed): `python3 bridge/selftest.py`
- Send test OSC (stdlib, no deps): `python3 bridge/tools/oscsend.py <host> <port> <address> <args…>`
- Read the console back over RCP: `printf 'get <PATH> <index> <sub>\n' | nc -w2 192.168.50.2 49280`

There is no build step and no CI. The bridge is plain Python 3 + `python-osc`.

## Architecture (`bridge/tf-bridge.py`)

One process, three concerns:

- **Console link** — one persistent *blocking* TCP socket with a reconnect loop. Networking is
  deliberately blocking: on the booth Mac an async runtime proved unreliable on the console's
  long-lived link, while blocking sockets are rock-solid. A guard (`is_tf_network`) refuses to send
  unless a local IP is on the `192.168.50.*` subnet — off the console network, cues are dropped.
- **Write path** — `send()` / `_raw_send()` serialise every console write behind one lock with a
  5 ms pacing floor, so concurrent fades and direct writes never interleave or outrun the console.
  Preserve this: correctness of fade-cancel depends on a superseding write setting the fade's stop
  flag *before* it takes `write_lock`.
- **OSC vocabulary** — two grammars share the dispatcher:
  - **Legacy `/tf/mute|unmute|scene`** — *stateful*. `desired_mute_state` vs `applied_mute_state`
    reconciliation, and mutes are **re-asserted on top of a recalled scene** (clear applied state on
    scene, re-sync). This is load-bearing show behavior; do not regress it. The token grammar
    (inputs / `aux` stereo-pairs / `matrix` / `dca` dual-write / `fx` / `st` / `master` / `sub`,
    with ranges, comma lists and `all`) lives in the `parse_*` / `expand_aux_token` helpers.
  - **New `/<strip>/<sel>/<param>`** — *direct*, fire-and-forget, last-writer-wins. `on_osc` parses
    it. Strips map via `STRIP_PATHS`; selectors (number / range / list / `all`) via
    `parse_range_list`; levels via `db_to_raw` (raw = hundredths of a dB, +10 dB ceiling = `1000`,
    −∞ sentinel = `-32768`); fades step at 30 Hz in a cancellable per-target thread (`plan` +
    `_run_fade`), curves in `curve_shape`; sends and labels follow the same builder pattern.

Adding a console parameter follows the existing pattern: build the RCP string, route it in `on_osc`
(or add a `/tf` handler), reuse `send()` / `set_level_direct()` / `start_fade()`. Keep the
integer-dB-scaling convention (`-6.00 dB` → raw `-600`).

## Repo layout

- `bridge/tf-bridge.py` — the service. `bridge/selftest.py` — offline tests.
- `bridge/OSC-REFERENCE.md` — full OSC address reference (both grammars).
- `bridge/docs/tf-bridge-guide.html` — operator manual + OSC dictionary (shareable page).
- `bridge/qlab/` — QLab cue reference (`QLAB-CUES.md`), the AppleScript generator
  (`generate.applescript`), and an agent-facing automation guide (`QLAB-AUTOMATION.md`).
- `bridge/tools/` — stdlib OSC senders. `bridge/reference/` — original `tf3.py` + plist.
- `bridge/DEPLOY.md`, `bridge/PENDING-LIVE.md` — deploy runbook and live-verification log.

`.qlab5` workspaces are binary and hold the theatre's show content; they are gitignored (reproduced
by `generate.applescript`).

## History

Evolved from the venue's `tf3.py`. An earlier **Rust** implementation (the `yamaha-rcp` crate,
upstream at `BrenekH/yamaha-rcp-rs`, plus a `yamaha-osc` bridge) was removed after it proved
unreliable on the booth Mac (a tokio long-lived-socket issue) and the project settled on the proven
blocking-Python bridge. If you need the RCP protocol details the Python encodes, read
`bridge/tf-bridge.py` and `bridge/OSC-REFERENCE.md` — they are now the source of truth.
