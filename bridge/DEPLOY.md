# Deploying `tf-bridge.py` to the theatre MacBook

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
- Local reference copies of the originals live in `bridge/reference/` (tf3.original.py, tf3.server.plist).

## Test alongside the live service (safe, no cutover)
Runs on a spare port so it can't disturb the running `tf3.py`:
```
scp bridge/tf-bridge.py theatretechs@192.168.50.218:~/Scripts/tf-bridge.py
ssh theatretechs@192.168.50.218 \
  'TF_OSC_PORT=9000 /Library/Frameworks/Python.framework/Versions/3.14/bin/python3 ~/Scripts/tf-bridge.py'
```
Then send OSC to `192.168.50.218:9000` and read the console back (see PENDING-LIVE.md).
Env overrides: `TF_OSC_PORT`, `TF_OSC_IP`, `TF_IP`, `TF_PORT`.

## Cutover (replace tf3.py on port 8000)
1. **Back up** the original on the Mac (if not already):
   `cp ~/Scripts/tf3.py ~/Scripts/tf3.py.orig`
2. **Install** the new bridge next to it:
   `scp bridge/tf-bridge.py theatretechs@192.168.50.218:~/Scripts/tf-bridge.py`
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

## Rollback (instant)
```
cp ~/Scripts/tf3.py.orig ~/Scripts/tf3.py     # restore original (if you overwrote tf3.py)
launchctl bootout gui/$(id -u)/tf3.server
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/tf3.server.plist
```
The plist, port, and console target never change, so rollback only swaps the script file back.

## Notes
- The bridge keeps tf3's `/tf/mute|unmute|scene` addresses working, so existing QLab cues are
  unaffected. Migrate cues to the richer grammar (OSC-REFERENCE.md) at your own pace.
- Config (console IP, OSC port, channel counts) is constants at the top of the file — techs edit
  the `.py` directly, or set the `TF_*` env vars in the plist's `EnvironmentVariables`.
