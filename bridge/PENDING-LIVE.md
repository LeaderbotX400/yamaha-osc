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
