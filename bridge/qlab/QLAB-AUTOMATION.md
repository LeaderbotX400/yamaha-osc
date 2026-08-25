# Driving QLab programmatically — a field guide for agents

Everything learned making a QLab 5 workspace build/fire cues for the TF bridge, written for the next
agent (or human) who needs to automate QLab. Concrete, with the dead-ends called out so you don't
re-walk them.

**TL;DR:** Use **AppleScript over `osascript`** for anything that creates/edits/fires cues — it's
reliable and local. QLab's **OSC API is real but was dead on this Mac** (macOS 15 networking bug +
passcode). `.qlab5` files are **binary plists** (NSKeyedArchiver), parseable but not worth
hand-authoring. Test target used here: **QLab 5.6.3 on macOS 15.6.1**, booth Mac
`theatretechs@192.168.50.218` (IP has changed before — verify).

---

## 1. Two automation surfaces

| Surface | Transport | Verdict here |
|---|---|---|
| **AppleScript** | Apple Events (local) | **Use this.** Reliable, no network involved, works over ssh once Automation is authorized. |
| **OSC API** | UDP/TCP 53000 | Powerful and scriptable, but returned *nothing* on this Mac (see §5). Needs a GUI fix + passcode. |
| **`.qlab5` file editing** | binary plist on disk | Readable with `plistlib` for recon; **do not** hand-author the object graph — let QLab write it. |

---

## 2. AppleScript: access & running

- **Automation permission** is already granted for Terminal/ssh → QLab on the booth Mac. The very
  first time a script touches a *workspace* (not just `application "QLab"`), QLab pops a one-time
  authorization dialog on screen; an unattended ssh call will **hang until someone clicks Allow**.
  App-level calls (`version`) work without it.
- **Run from a file, not a heredoc.** Quoting AppleScript through an ssh heredoc mangles fast. Write a
  `.applescript` file, `scp` it over, `osascript /tmp/foo.applescript`. Always wrap the ssh call in a
  `timeout` — a blocked script otherwise hangs for minutes.

```bash
scp foo.applescript theatretechs@HOST:/tmp/foo.applescript
timeout 30 ssh theatretechs@HOST 'osascript /tmp/foo.applescript'
```

- Sanity ping (no workspace access, never blocks): `osascript -e 'tell application "QLab" to version'`
  → `5.6.3`.

---

## 3. AppleScript cookbook (verified on 5.6.3)

### Cue type names
`make type "Network"` for an OSC/network cue. **`"OSC"` is rejected** (that's the QLab 3 name).
`q type of aCue` returns the readable type: `"Network"`, `"MIDI"`, `"Group"`, `"Cue List"`, …

### Creating a cue — grab it from the selection
`make` and `duplicate` **do not return a usable reference** (binding the result throws `-2753`/`-10006`).
The new cue becomes selected, so grab it right after:

```applescript
tell application "QLab"
  tell front workspace
    make type "Network"
    set c to last item of (selected as list)   -- THE idiom. Always.
    set q number of c to "501"
    set q name of c to "CH 1 - fader to dB"
    set custom message of c to "/ch/1/fader 0.0"
  end tell
end tell
```

### Settable cue properties that matter
| Property | Notes |
|---|---|
| `q name` | display name (string) |
| `q number` | cue number (string; may be "") |
| `custom message` | for a Network cue, the raw OSC string it sends, e.g. `/ch/1/fader 0.0` |
| `q type` | read-only; the readable type name |

### Grouping — `move`, not `duplicate to`
`move <cue> to end of <group>` **works**. `duplicate <cue> to <loc>` and `move <cue> to <cue>`
(replace) **do not** (`-10006`). Pattern:

```applescript
make type "Group"
set grp to last item of (selected as list)
set q name of grp to "TF BRIDGE TEMPLATES"
-- ...for each cue: make it, set props, then:
move c to end of grp
```

### The destination patch is NOT settable (the big limitation)
On 5.6.3 a Network cue's `patch` cannot be set via AppleScript. Every attempt fails:
`set patch of c to 2` → *"Can't set patch of network cue to any"*; `to "TF3"` → *"Can't make TF3 into
type integer"*; there is no scriptable `network patches` collection to grab a patch object from;
selecting a correctly-patched cue before `make` does **not** make the new cue inherit it.

**Workaround (what we did):** make the bridge's OSC patch the **first** network patch in the
workspace (Settings → Network → OSC; we deleted the old loopback "Patch 1" so `TF3 → 127.0.0.1:8000`
is first). New Network cues default to the first patch, so they land on the bridge automatically. If
you can't reorder patches, the fallback is a one-time manual step: select the cues, set Destination
in the inspector.

### Reading the workspace
```applescript
tell application "QLab" to tell front workspace
  set n to count of cues            -- NB: this is FLATTENED (includes cues nested in groups)
  repeat with c in cues
    -- q type / q number / q name / custom message of c
  end repeat
end tell
-- which file am I editing? (safety before mutating)
tell application "QLab"
  count of workspaces
  repeat with w in workspaces
    name of w & "  " & (path of w)   -- name is the stored workspaceName; path is the file
  end repeat
end tell
```
`cues of front workspace` returns a **flattened** list (top-level + group children). To walk a group
specifically, use `cues of aGroupCue`.

### Deleting cues — collect ids first, then delete by id
Deleting while iterating `cues` corrupts the loop (`-10006`). Snapshot `uniqueID`s, then delete:

```applescript
set killIds to {}
repeat with c in (cues of front workspace)
  if (q name of c) is "ZZTEST" then set end of killIds to (uniqueID of c)
end repeat
repeat with kid in killIds
  delete (first cue whose uniqueID is (kid as string)) of front workspace
end repeat
```

### Firing a cue (for verification)
```applescript
tell application "QLab" to tell front workspace
  repeat with c in cues
    if (q number of c) is "505" then start c
  end repeat
end tell
```

### Opening / saving
`open POSIX file "/path/to/x.qlab5"` opens a workspace and makes it front (QLab keeps multiple open;
verify with `path of front workspace`). `save` (inside `tell front workspace`) writes it to disk.
There's no reliable scriptable "Save As" — copy the file first, `open` the copy, then `save`.

### Probing unknown terminology safely
AppleScript rejects unknown terms at **compile** time, so you can't `try` an unknown property. Use
`run script "…"` — it compiles at *runtime*, turning unknown-term errors into catchable ones:

```applescript
try
  set n to (run script "tell application \"QLab\" to return count of (network patches of front workspace)")
on error e number en
  -- -2741 / -2740 = term not recognized; -1700 = wrong type
end try
```

---

## 4. Safe-workflow notes (don't wreck a real show file)

- Work in a **copy**: `cp base.qlab5 wip.qlab5`, `open` the copy, build, `save`. Originals untouched.
- In-memory changes aren't on disk until `save`. If you pollute the live workspace during probing,
  the cheap fix is telling the user to **Revert to Saved** — or delete your cues by id (§3).
- Probe cues you `make` but don't clean up become **orphans** (they show blank name, auto-numbered
  like `1.5`, `1.6`). Always delete what you make, or work in a throwaway workspace.

---

## 5. The OSC API (why it was dead here, and how to revive it)

QLab has a full OSC control API on **UDP/TCP 53000**. It should let you `/workspace/{id}/new
"network"`, set `custom message`, etc. On this Mac it returned **no replies to anything**, even over
loopback. Findings:

- **Passcode required.** The workspace has an OSC-access passcode (**`1391`**, accessMask 7 = full).
  A client must send `/connect "1391"` first; QLab's own cues authenticate with `userData:"1391"`.
- **Replies are conditional.** QLab only replies when a message generates one; `/alwaysReply 1`
  forces replies per-connection. Even so — nothing came back.
- **The real blocker: a documented macOS 15 bug** where QLab's local-network access must be toggled
  after each restart or its OSC networking is dysfunctional. This Mac is macOS 15.6.1. Fix is a GUI
  toggle (System Settings → Privacy & Security → Local Network → QLab off/on, and check
  Settings → Network / OSC access is enabled in QLab). Until then, prefer AppleScript.
- **From WSL/off-box you won't hear replies anyway** — QLab replies to the source ip:port over UDP,
  which WSL2's NAT doesn't return. Run OSC-API clients **on the Mac (loopback)**, or use TCP.
- Address shapes (from Figure 53 docs): `/new "network"` returns the new cue's unique id;
  `/workspace/{id}/move/{cueId} {index} {parentId}`; `/cue/{id}/name`, `/cue/{id}/number`;
  select via `/workspace/{id}/select_id/{id}`; save with `/workspace/{id}/save`. The network-cue
  message/patch control addresses are not well documented — AppleScript's `custom message` is the
  reliable lever.

A minimal stdlib OSC client (send `/connect`, then commands on one socket, print replies) lives in
`../tools/oscq.py` / `../tools/oscsend.py`.

---

## 6. The `.qlab5` file format (for recon only)

A `.qlab5` is an **Apple binary property list** encoded with **NSKeyedArchiver** (`$archiver`,
`$objects`, `$top`, `$version`) — *not* SQLite. Parse it read-only with Python:

```python
import plistlib
d = plistlib.load(open("x.qlab5", "rb"))
objs = d["$objects"]                       # flat object table; refs are plistlib.UID -> index
def classname(o):
    return objs[o["$class"].data].get("$classname") if isinstance(o, dict) and "$class" in o else None
```

Useful landmarks:
- A Network cue's class is `OSCCue`; groups are `GroupCue`. (Class-dedup means a naive histogram can
  *undercount* — don't trust object counts, trust QLab.)
- The OSC message lives under the cue's `allParameterValues` → `com.figure53.oscmessage`
  (`custom` string) with `oscString = "{custom}"`; the destination is a named network patch
  (`f53.lastSeenPatchInfo` holds a cached `{useTcp, host, port}`).
- Workspace network patches live in settings under key `networkPatches`: an array of
  `{buildNumber, data:{…, host, port, name}, identifier:"com.figure53.qlab.settings.network.patch"}`.
  This Mac's template ships one named **TF3** → `host 127.0.0.1`, `port 8000`.
- Remote/OSC access config: `passcodeEntries` (RemoteAccessPasscodeEntry `{passcode, accessMask}`),
  `oscListenerPort` (53000), `udpPlainTextPort` (53535).

Hand-editing this graph (new UIDs, class refs, patch linkage) is fragile and QLab may rewrite it on
save. Rewriting a single scalar (e.g. a patch `port`) is comparatively safe, but prefer AppleScript.

---

## 7. Gotcha checklist (paste-me summary)

- [ ] Cue type is `"Network"`, never `"OSC"`.
- [ ] After `make`/`duplicate`: `set c to last item of (selected as list)`.
- [ ] Group with `move c to end of grp`; `duplicate … to` is broken.
- [ ] `patch` isn't settable → make the bridge patch **first** so cues default to it.
- [ ] `cues of workspace` is flattened; walk groups via `cues of grp`.
- [ ] Delete by collecting `uniqueID`s then `delete (first cue whose uniqueID is …)`.
- [ ] Probe unknown terms via `run script` (runtime compile).
- [ ] Always `timeout` the ssh, and expect a one-time GUI auth dialog on first workspace access.
- [ ] Work in a **copy**; `save` is the only thing that writes disk.
- [ ] OSC API needs `/connect "1391"` + the macOS-15 Local-Network toggle; else use AppleScript.

See also: `QLAB-CUES.md` (copy-paste cue reference), `generate.applescript` (the working generator),
`../docs/tf-bridge-guide.html` (the operator manual + OSC dictionary).
