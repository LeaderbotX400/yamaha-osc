# Live verification — ALL PASSED (2026-08-25, reconnected to Mac + console TF3)

Re-ran the test instance on port 9000 beside the live tf3, drove it with a stdlib OSC sender from
WSL, and read the console back over `nc`. **Every checklist item below passed live.** The bridge
held the console link with zero drops across all tests.

```
TF_OSC_PORT=9000 /Library/Frameworks/Python.framework/Versions/3.14/bin/python3 ~/Scripts/tf-bridge.py &
```
Console readback: `printf 'get <PATH> <index> <sub>\n' | nc -w2 192.168.50.2 49280`

## Checklist — all verified 2026-08-25
Restore each channel afterwards. Used ch5 + spares; all restored.

- [x] **Exponential fade curve** — `/ch/5/fade -30 1500 exp` eased in (tiny early steps, `t²`), landed −3000.
- [x] **Send on** — `/ch/5/send/mix/3/on 0|1` → `InCh/ToMix/On 4 2` toggled 1→0→1.
- [x] **Send fade** — `/ch/5/send/mix/3/fade -20 1500` → `.../ToMix/Level 4 2` ramped to −2000.
- [x] **Mute group** — `/mutegroup/1 1|0` → `MuteMaster/On 0 0` toggled 0→1→0.
- [x] **DCA dual-write** — `/dca/1/on 0|1` and `/dca/1/mute` wrote both `DCA/Fader/On 0 0` and
      `DcaCh/Fader/On 0 0` together.
- [x] **Other strips** — st (`St 0`), mono (`Mono 0`), stin (`StInCh 0`), fxrtn (`FxRtnCh 0`),
      mix (`Mix 2`), mtrx (`Mtrx 0`) each took −6 dB (−600) and restored to base.
- [x] **Label with spaces** — `/ch/5/name "Lead Vox"` → `InCh/Label/Name 4 0` returned `"Lead Vox"`.
- [x] **Fade from unknown start** (`read_level_short`) — set ch5 to −600 directly, restarted the
      bridge (cleared shadow), `/ch/5/fade -30 2000` ramped from −640 down to −3000, no jump, no
      `read_level_short failed`.
- [x] **Scene re-mute-after-recall** — recalled A10, `/tf/mute 5` (On→0), recalled A10 again;
      log showed `SCENE: A10 / ssrecall_ex scene_a 10` then `SYNC ('input',5) None->0` →
      `SEND .../Fader/On 4 0 0`. ch5 stayed muted on top of the fresh scene.
- [x] **Concurrent fades pacing** — `/ch/1-8/fade -40 3000` ramped all 8 together to −4000; bridge
      log showed paced, interleaved sends across ch0–7; clean restore. No lag/queueing.

## Production cutover — DONE (2026-08-25)
Cut over per `DEPLOY.md`: backed up `tf3.py` → `tf3.py.orig`, swapped `~/Scripts/tf3.py` for the new
bridge, reloaded the `tf3.server` launchd agent. New bridge live on **UDP :8000** (pid via launchd,
framework Python), `TF CONNECTED`. Verified on :8000: new grammar (`/ch/5/mute`, `/ch/5/fader -6`)
and legacy `/tf/*` both drive the console. Rollback still = restore `tf3.py.orig` + reload.

Still worth doing: a full show smoke test from QLab during a real rehearsal, and confirming a long
idle→active cycle holds the link (expected fine — blocking Python is proven here).

## Cleanup — DONE (2026-08-25)
- Killed the :9000 test instance; removed `~/yamaha-osc-bridge/tf-bridge-test.py`.
- Console spare channels (ch5 etc.) restored. Console left in scene A10 (SONATA_SCHOOL, non-YMCA).
- Remaining (cosmetic, low priority): sanity-check ch2 label/colour from earlier Rust-era tests.

## KNOWN ISSUE / POTENTIAL PROBLEM (installed 2026-11-16 21:15 with this open)
The new bridge went live on the booth Mac with **scene re-assert after a recall NOT verified on the real console**
(a recall overwrites the whole console, so it was never tested live; it is covered offline only).
Two ways it could bite, so watch for them the first time a scene is recalled during a show:
1. **A mic you cut could come back** (or stay in the scene's state) if the re-assert doesn't land after the recall.
2. **Behaviour change:** on/off from `/ch|mix|mtrx|dca/.../on|mute|only` is now *remembered* (like `/tf/mute`). A scene
   recall re-applies the **latest remembered on/off for every channel touched since the bridge started** over the
   scene's own values. If the show relies on console scenes to set mics, those remembered states win. Restarting the
   bridge (or `launchctl` reload) clears the memory.
**To verify:** mute `/ch/36/on 0`, recall a scene (e.g. the current one), confirm ch36 is still off, then restore.
**To back out:** `cp ~/Scripts/tf3.py.prev ~/Scripts/tf3.py` then reload `tf3.server` (see DEPLOY.md).

## Live verification — batching / fade engine / mic scenes (2026-11-16, TF3 + booth Mac, test instance :9000)
Run on spare channels (28-32, 35-40; all restored to baseline afterwards). The booth bridge on :8000 was
**not** replaced; `install.sh` has not been run.

- [x] **Batching accepted** — 24-ch writes as 3x8-line TCP writes all applied, no errors. Console takes
      >1000 cmd/s (chunk 8 / 5 ms), so `TF_BATCH_MAX=8`, `TF_PACING_MS=5` are safe defaults.
- [x] **24-channel fade** — 3.03 s for a 3 s fade (86 ticks), lands on target. (Measure with the bridge's
      own log: the console floods every client with NOTIFYs, which makes external polling look slow.)
- [x] **`/panic`** and **`/ch/x/fade/stop`** — fade stops in place and stays.
- [x] **`only`**, **`|` groups**, **legacy `/tf/mute`** — read back correct on the console.
- [x] **Snapshot save/recall** — levels restored after a 600 ms crossfade (an earlier odd reading was a stale readback).
- [x] **`fader/rel`**, **preset** end to end. **Reply format** — `OK get ... <int>`; errors are `ERROR set InvalidArgument`.
- [x] `fader/raw` now clamps to the +10 dB ceiling (the console clamped it anyway; the bridge now matches).
- [ ] **Scene re-mute after a scene recall** — NOT run (a recall overwrites the whole console). Offline-tested only
      (one-burst re-assert). Verify when the console can be disturbed: mute via `/ch/36/on 0`, recall a scene, check it stays muted.
- [ ] **`CONSOLE ERROR` log line** — format confirmed with `nc`; not yet seen in the bridge log (the grammar can't easily send a bad command).

### Finding about the cue list (not a bridge bug)
`/ch/19-21/on on`-style messages write only the named channels (verified on the live :8000 bridge with ch36-39).
Channel 1 going off comes from cues like `/ch/1-24/on off` ("MUTE") and `/ch/1-24/mute 1` ("TOP OF SHOW") in the
show's cue list. Use `/ch/1-24/only <list>` when "only these mics" is what's meant.
