# CLAUDE.md

Guidance for Claude Code (claude.ai/code) working in this repository.

## What this is

An **OSC → Yamaha TF console bridge**. `src/tf-bridge.py` is a small Python service that listens
for OSC (from QLab) and drives a Yamaha TF mixing console over Yamaha's Remote Control Protocol (RCP,
newline-terminated ASCII over TCP). It runs on the theatre's booth Mac beside QLab, translating cue
messages into console moves: faders in dB, curved fades, mutes, sends, labels, mute groups, scenes,
"only these mics" cuts, presets and snapshots.

Developed and run against a **Yamaha TF3** at `192.168.50.2:49280`. The bridge listens on UDP
`0.0.0.0:8000`; QLab reaches it on loopback (`127.0.0.1:8000`).

## Where it runs

The service is deployed to the booth Mac (`theatretechs@192.168.50.x`) and kept alive by a user
launchd agent `tf3.server` (`~/Library/LaunchAgents/tf3.server.plist`), which runs `~/Scripts/tf3.py`
(that filename now holds this bridge; the venue's original is preserved as `~/Scripts/tf3.py.orig`).
Interpreter: `/Library/Frameworks/Python.framework/Versions/3.14/bin/python3` (it has `python-osc`;
the bare `python3` in an ssh shell does not). Logs: `/tmp/tf3.out.log`, `/tmp/tf3.err.log`.
Deploy/rollback runbook: `src/docs/DEPLOY.md`.
`presets.json` / `snapshots.json` live beside the deployed script (`~/Scripts/`); both are gitignored show data.

## License
All code is proprietary (c) 2026 Eric Singer, restricted distribution, explicit revocable grants only (see `LICENSE`).
Keep the two-line copyright header on every new source file; `src/reference/` is third-party and excluded.

## Open issue
The new bridge is **installed on the booth Mac (2026-11-16)** but scene re-assert after a recall is not yet verified on the
real console, and new-grammar on/off is remembered across scenes. See `src/docs/PENDING-LIVE.md` → KNOWN ISSUE.
Rollback: `~/Scripts/tf3.py.prev` (previous) or `.orig` (venue original).

## Commands

- Offline logic tests (no console, no `python-osc` needed; uses a fake socket): `python3 src/selftest.py`
- Send test OSC (stdlib, no deps): `python3 src/tools/oscsend.py <host> <port> <address> <args…>`
  (`src/tools/oscq.py` queries OSC replies.)
- Test rig without a console: run a tiny fake RCP TCP server that answers `OK set …` / `OK get …`, then
  start the bridge with `TF_IP=127.0.0.1 TF_PORT=<p> TF_OSC_PORT=<q> TF_OSC_IP=127.0.0.1 TF_SKIP_NET_GUARD=1`.
  `TF_SKIP_NET_GUARD` is test-only; never set it on the Mac.
- Read the console back over RCP: `printf 'get <PATH> <index> <sub>\n' | nc -w2 192.168.50.2 49280`

There is no build step and no CI. The bridge is plain Python 3 + `python-osc`.

## Architecture (`src/tf-bridge.py`)

One process, three concerns:

- **Console link** — one persistent *blocking* TCP socket with a reconnect loop. Networking is
  deliberately blocking: on the booth Mac an async runtime proved unreliable on the console's
  long-lived link, while blocking sockets are rock-solid. A guard (`is_tf_network`) refuses to send
  unless a local IP is on the `192.168.50.*` subnet — off the console network, cues are dropped.
- **Write path** — the console takes one ordered command stream, so "parallel" means *batching*, not
  more sockets. `send_many()` / `emit()` write commands `TF_BATCH_MAX` (8) lines per TCP write, paced
  `TF_PACING_MS` (5 ms) apart, all behind one `write_lock`. Handlers build a command list per OSC
  message and send it once; `begin_collect()`/`end_collect()` (thread-local) make a whole preset one batch.
- **Fade engine** — one daemon thread (`_fade_engine` → `fade_tick`) advances *every* `active_fades`
  entry at 30 Hz and writes the tick as one batch. Values come from `fade_value()` (a function of elapsed
  time, so load never stretches a fade). **Invariant to preserve:** anything that supersedes a fade pops
  it from `active_fades` (fade_lock only) *before* it takes `write_lock`, and `fade_tick` takes
  `write_lock` *before* snapshotting — so a cancelled fade can never write after the write that cancelled
  it. Lock order is always `write_lock` → `fade_lock`. `selftest.py` covers this ordering.
- **Console replies** — a reader thread per connection (`_reader`; `TF_READER=0` disables) logs
  `CONSOLE ERROR`, resolves `get_many()` waiters (one batched read for unknown fade-start levels,
  snapshots, `fader/rel`) and keeps `fader_shadow` current from `NOTIFY` (never for a target a fade owns).
- **OSC vocabulary** — two grammars share the dispatcher:
  - **Legacy `/tf/mute|unmute|scene`** — *stateful*. `desired_mute_state` vs `applied_mute_state`
    reconciliation, and mutes are **re-asserted on top of a recalled scene** (clear applied state on
    scene, re-sync). Load-bearing show behavior; do not regress it. `synchronize_states()` applies all
    pending keys as ONE burst and `sync_wake` makes it run immediately (no 1 s tick). The token grammar
    (inputs / `aux` stereo-pairs / `matrix` / `dca` dual-write / `fx` / `st` / `master` / `sub`, with
    ranges, comma lists and `all`) lives in the `parse_*` / `expand_aux_token` helpers.
  - **New `/<strip>/<sel>/<param>`** — `on_osc` parses it. Strips map via `STRIP_PATHS`; selectors
    (number / range / list / `all`, plus `a|b` **groups** whose values pair by position) via
    `split_groups` / `values_per_group`; levels via `db_to_raw` (raw = hundredths of a dB, +10 dB
    ceiling = `1000`, −∞ sentinel = `-32768`); curves in `curve_shape`. Levels/fades/sends/labels are
    *direct* (last-writer-wins). **`on` / `mute` / `only` on `ch`/`mix`/`mtrx`/`dca` are stateful** — they
    go through `set_on_pairs` → `update_state` into the same desired-state map as `/tf/mute`, so scene
    recalls re-assert them; `stin`/`fxrtn`/`st`/`mono` on/off stay direct.
  - **Virtual DCAs** — the TF's RCP has no DCA-assignment parameter (checked: its 108 parameters, no match), so
    `/vdca/<n>/only|add|remove|clear|fader|fade` keep bridge-side groups (`vdca_*`): level = base + group offsets,
    direct fader writes re-base a member (`vdca_note_direct`). In-memory only; scene recalls overwrite levels.
  - **Scene-only design (current show)** — one flat QLab cue per number sends `/scene/recall a <75+n>` (scenes A76-A97, stored on
    the desk, carry mic on/off and DCA layout); `build_mic_cues.py` generates/migrates them. Never store/overwrite a scene without
    explicit approval for that slot. The send `only`/`prepost` verbs and `/vdca` remain in the code but the show doesn't use them.
    `/scene/recall` forgets the remembered on/off first (`forget_remembered_states`) so the scene wins; `keep` or `/tf/scene` re-assert.
  - **Extras** — `only` (mic cut), `fader/rel`, `fade/stop`, `/panic`, `/preset/<name>`
    (`presets.json`, lines run through `dispatch`), `/snapshot/save|recall` (`snapshots.json`).

Adding a console parameter follows the existing pattern: build the RCP string, route it in `on_osc`
(or add a `/tf` handler), reuse `emit()` / `set_levels_direct()` / `start_fades()` / `set_on_pairs()`;
support `|` groups via `split_groups` + `values_per_group`. Keep the
integer-dB-scaling convention (`-6.00 dB` → raw `-600`).

## Repo layout

- `src/tf-bridge.py` — the service. `src/selftest.py` — offline tests. `src/presets.example.json` — preset template.
- `src/install.sh` — builds/pushes a self-contained installer (`sh src/install.sh user@host`, or `--print | sh`);
  backs up, reloads launchd, auto-rolls-back. Restarts the live bridge, so not mid-show.
- `src/docs/OSC-REFERENCE.md` — full OSC address reference (both grammars).
- `src/docs/tf-bridge-guide.html` — operator manual + OSC dictionary (shareable page).
- `src/docs/DEPLOY.md`, `src/docs/PENDING-LIVE.md` — deploy runbook and live-verification log.
- `src/qlab/` — QLab cue reference (`QLAB-CUES.md`), the AppleScript generators (`generate.applescript`,
  `generate_eos_triggers.applescript`), and agent guides (`QLAB-AUTOMATION.md`, `QLAB-EOS.md`).
- `src/tools/` — stdlib OSC senders (`oscsend.py`, `oscq.py`). `src/reference/` — original `tf3.py` + plist.

When you change the OSC vocabulary, update `OSC-REFERENCE.md`, `QLAB-CUES.md` and `tf-bridge-guide.html`
together, and extend `selftest.py`.

`.qlab5` workspaces are binary and hold the theatre's show content; they are gitignored (reproduced
by `src/qlab/generate.applescript`).

## History

Evolved from the venue's `tf3.py`. An earlier **Rust** implementation (the `yamaha-rcp` crate,
upstream at `BrenekH/yamaha-rcp-rs`, plus a `yamaha-osc` bridge) was removed after it proved
unreliable on the booth Mac (a tokio long-lived-socket issue) and the project settled on the proven
blocking-Python bridge. If you need the RCP protocol details the Python encodes, read
`src/tf-bridge.py` and `src/docs/OSC-REFERENCE.md` — they are now the source of truth.
