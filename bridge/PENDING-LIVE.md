# Pending live verification (needs the console)

Went offline mid-testing. The core was live-verified (see the plan's STATUS section); the items
below were NOT exercised against the real TF3 yet. Re-run the test instance on port 9000 first:

```
TF_OSC_PORT=9000 /Library/Frameworks/Python.framework/Versions/3.14/bin/python3 ~/Scripts/tf-bridge.py &
```
Send OSC with the repo's `osc_send` example (from a machine that can reach the Mac):
`cargo run --example osc_send -- 192.168.50.218:9000 <address> <args...>`
Read the console back with netcat: `printf 'get <PATH> <index> <sub>\n' | nc -w2 192.168.50.2 49280`

## Checklist
Restore each channel afterwards. Use spare channels, not ones in the show.

- [ ] **Exponential fade curve** — `/ch/5/fade -30 1500 exp`; confirm it ramps (eases in, `t²`).
- [ ] **Send on** — `/ch/5/send/mix/3/on 0|1` → `get MIXER:Current/InCh/ToMix/On 4 2`.
- [ ] **Send fade** — `/ch/5/send/mix/3/fade -20 1500` → `.../ToMix/Level 4 2` ramps.
- [ ] **Mute group** — `/mutegroup/1 1|0` → `get MIXER:Current/MuteMaster/On 0 0`.
- [ ] **DCA dual-write** — `/dca/1/on 0` → both `get MIXER:Current/DCA/Fader/On 0 0` and
      `.../DcaCh/Fader/On 0 0` become 0. `/dca/1/mute 1` too.
- [ ] **Other strips** — `/st/1/fader -6` (`St` 0), `/mono/1/fader -6` (`Mono` 0),
      `/stin/1/on 0` (`StInCh` 0), `/fxrtn/1/fader -6` (`FxRtnCh` 0),
      `/mix/3/fader -6` (`Mix` 2), `/mtrx/1/fader -6` (`Mtrx` 0).
- [ ] **Label with spaces** — `/ch/5/name "Lead Vox"` → `get MIXER:Current/InCh/Label/Name 4 0`
      returns `"Lead Vox"` (confirm the space survives; QLab sends it as one string arg).
- [ ] **Fade from unknown start** (`read_level_short`) — restart the bridge (clears the fader
      shadow), then `/ch/5/fade -30 2000` with no prior `/fader`; confirm it reads the current level
      and ramps smoothly from it (not a jump). Watch the log for `read_level_short failed`.
- [ ] **Scene re-mute-after-recall** (the valuable stateful behavior) — `/tf/mute 5`, then
      `/scene/recall a <safe-scene>` (or `/tf/scene A<n>`); confirm ch5 is muted again *after* the
      scene loads (tf3 clears applied state on scene and re-asserts mutes). Use a scene you know.
- [ ] **Concurrent fades pacing** — start fades on several channels at once
      (`/ch/1-8/fade -40 3000`); confirm all ramp smoothly and the log shows paced, interleaved
      sends (no console lag/queueing).

## Then: production cutover
Follow `DEPLOY.md`. After cutover on :8000, run a full show smoke test from QLab (existing `/tf/*`
cues + any newly-migrated `/ch/*` cues), and confirm a long idle→active cycle stays connected.

## Cleanup still owed from the offline session
- `pkill -f tf-bridge` on the Mac (a test instance on :9000 was left running).
- Remove `~/yamaha-osc-bridge/tf-bridge-test.py` (stale sed copy).
- Sanity-check console state from earlier Rust-era tests: ch2 label/colour, input faders 1–16.
