# Deploying `tf-bridge.py` to the theatre MacBook

> **Status: cut over 2026-08-25.** The new bridge is live on UDP :8000 via the `tf3.server` launchd
> agent (`~/Scripts/tf3.py` now holds the new bridge; original preserved as `~/Scripts/tf3.py.orig`).
> The steps below are the runbook used, kept for re-deploys and rollback.

The bridge is plain Python 3 + `python-osc` — both already on the Mac. Deployment is copying one
file and reloading the existing launchd agent. No build, no toolchain, no cross-compile.

## Facts about the target
- Host: `theatretechs@192.168.50.218` (console `192.168.50.2:49280`). IP may change — check.
- Interpreter that has `python-osc`: `/Library/Frameworks/Python.framework/Versions/3.14/bin/python3`
  (the plain `python3` in an SSH shell does **not** have it).
- Existing service: user LaunchAgent `~/Library/LaunchAgents/tf3.server.plist`
  (label `tf3.server`, `RunAtLoad`+`KeepAlive`) → runs `/Users/theatretechs/Scripts/tf3.py`.
  Managing it needs **no sudo**. Logs: `/tmp/tf3.out.log`, `/tmp/tf3.err.log`.
- OSC listen port QLab uses: **UDP 8000**. Console link is wired (en5). Only one process may bind 8000.
- Local reference copies of the originals live in `src/reference/` (tf3.original.py, tf3.server.plist).

## One-command install (`src/install.sh`)
Does the cutover steps below in one go. **It restarts the live bridge — don't run it mid-show.**
```
sh src/install.sh theatretechs@192.168.50.218              # push over ssh and install
sh src/install.sh --print | ssh theatretechs@192.168.50.218 sh -s --    # same, piped by hand
sh src/install.sh --print > install-tf-bridge.sh           # self-contained; run it on the Mac with sh
```
Flags (after the host, or after `sh -s --`): `--no-restart` (files only), `--force` (reinstall even
if identical). It checks `python-osc` and that the bridge compiles, keeps `tf3.py.orig` (once) and
`tf3.py.prev`, installs as `~/Scripts/tf3.py`, creates `~/Scripts/presets.json` only if missing,
reloads `tf3.server`, waits for `OSC server listening`, and **rolls back to `.prev` automatically** if
it doesn't come up. Safe to re-run; an identical install is a no-op.

## Test alongside the live service (safe, no cutover)
Runs on a spare port so it can't disturb the running `tf3.py`:
```
scp src/tf-bridge.py theatretechs@192.168.50.218:~/Scripts/tf-bridge.py
ssh theatretechs@192.168.50.218 \
  'TF_OSC_PORT=9000 /Library/Frameworks/Python.framework/Versions/3.14/bin/python3 ~/Scripts/tf-bridge.py'
```
Then send OSC to `192.168.50.218:9000` and read the console back (see PENDING-LIVE.md).
Env overrides: `TF_OSC_PORT`, `TF_OSC_IP`, `TF_IP`, `TF_PORT`, plus tuning `TF_BATCH_MAX` (default 8
commands per TCP write), `TF_PACING_MS` (default 5), `TF_READER=0` (stop reading console replies —
fallback if the reader ever misbehaves on the booth Mac), `TF_PRESETS`, `TF_SNAPSHOTS`.
`TF_SKIP_NET_GUARD=1` is for the fake-console test rig only — never set it on the Mac.

## Cutover (replace tf3.py on port 8000)
1. **Back up** the original on the Mac (if not already):
   `cp ~/Scripts/tf3.py ~/Scripts/tf3.py.orig`
2. **Install** the new bridge next to it:
   `scp src/tf-bridge.py theatretechs@192.168.50.218:~/Scripts/tf-bridge.py`
   `python3 -m py_compile ~/Scripts/tf-bridge.py`  (sanity check)
3. **Point launchd at it.** Simplest is to overwrite the script the plist already runs:
   `cp ~/Scripts/tf-bridge.py ~/Scripts/tf3.py`  (keeps the plist unchanged)
   — or edit the plist's `ProgramArguments` to name `tf-bridge.py` instead.
4. **Reload the agent:**
   ```
   launchctl bootout gui/$(id -u)/tf3.server
   launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/tf3.server.plist
   ```
5. **Verify:** `tail -f /tmp/tf3.out.log` shows `OSC server listening on 0.0.0.0:8000` and
   `TF CONNECTED`. Fire representative cues from QLab and confirm the console reacts.

## Presets & snapshots (optional)
`presets.json` and `snapshots.json` live **beside the script** (`~/Scripts/`; `tf3.py` is the script,
so that is `~/Scripts/presets.json`). Start from `src/presets.example.json`:
`scp src/presets.example.json theatretechs@192.168.50.218:~/Scripts/presets.json`. They are
show content, so they are gitignored; the bridge writes `snapshots.json` itself on `/snapshot/save`
(make sure the launchd user can write `~/Scripts/`). A redeploy of the script never touches them.

## Rollback (instant)
```
cp ~/Scripts/tf3.py.orig ~/Scripts/tf3.py     # restore original (if you overwrote tf3.py)
launchctl bootout gui/$(id -u)/tf3.server
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/tf3.server.plist
```
The plist, port, and console target never change, so rollback only swaps the script file back.

## Known issue (open)
Scene re-assert after a recall is **not yet verified live**, and new-grammar on/off is now remembered across scene
recalls. Details and how to verify/back out: `PENDING-LIVE.md` → *KNOWN ISSUE*. The booth bridge was upgraded
2026-11-16 (`tf3.py.prev` = the previous version).

## Notes
- The bridge keeps tf3's `/tf/mute|unmute|scene` addresses working, so existing QLab cues are
  unaffected. Migrate cues to the richer grammar (OSC-REFERENCE.md) at your own pace.
- Behavior changes vs the first cutover: console writes are batched and fades share one engine
  thread; the bridge now reads console replies (logs `CONSOLE ERROR`, tracks real levels); on/off
  from the new grammar for ch/mix/mtrx/dca is stateful like `/tf/mute`. Live-verify a 24-channel
  mute burst, a 24-channel fade and the scene re-mute after upgrading (see PENDING-LIVE.md).
- Config (console IP, OSC port, channel counts) is constants at the top of the file — techs edit
  the `.py` directly, or set the `TF_*` env vars in the plist's `EnvironmentVariables`.
