# Triggering ETC Eos lighting from a song in QLab

Fire ETC Eos lighting cues automatically at specific moments in a song. QLab plays the song and, at
each marked point, sends an OSC message to the Eos console telling it to jump to a specific cue.

- **Eos console:** `192.168.50.175`, OSC on UDP **8000**. In QLab this is the **ETC** network patch.
- **What QLab sends:** `/eos/cue/<n>/fire` — fires Eos cue `<n>` in the current list (order‑independent,
  deterministic). Nothing is *written* to Eos; you program the looks there, QLab just calls them.
- **How the timing works:** a **Timeline Group** — the song is the group's first cue, and each
  lighting trigger is a Network cue whose **pre‑wait** equals its time in the song. Fire the group
  and everything runs on the song's clock.

`generate_eos_triggers.applescript` (next to this file) builds all those trigger cues for you from
the song's **slice markers**.

---

## The workflow

1. **Build a Timeline Group.** New Group cue → Inspector → **Mode: Timeline**. Put the song (an Audio
   cue) inside it as the **first** cue.

2. **Mark the trigger points.** Select the song cue and add a **slice marker** at every moment a
   light change should happen (the slice tools live under the cue's waveform / Cue ▸ Slices). The
   slice times become the trigger times — you can nudge them on the waveform later.

3. **Generate the triggers.** Select the Timeline Group and run:
   ```
   osascript generate_eos_triggers.applescript
   ```
   It creates one Network cue per slice: message `/eos/cue/<n>/fire`, pre‑wait set to the slice time,
   named `Eos GO cue <n> @ <time>s`. Numbering starts at `EOS_START` (default 1) and steps by
   `EOS_STEP` — edit those at the top of the script (e.g. `EOS_STEP : 0.5` for half‑number cues, or
   set `EOS_LIST` to send `/eos/cue/<list>/<n>/fire`).

4. **Point them at Eos (one‑time).** Select the new Eos cues and set **Destination → ETC** in the
   inspector. QLab 5.6.3 can't set a cue's patch from AppleScript, so new cues land on the first
   patch (the TF bridge) until you retarget them. Do this once per batch.

5. **Program the looks in Eos.** Record Eos cues `1, 2, 3 …` (matching the numbers above) to hold the
   lighting for each moment. QLab only *calls* them.

6. **Run it.** Fire the Timeline Group. The song plays; at each slice, QLab sends `/eos/cue/<n>/fire`
   and Eos jumps to that cue.

Re‑marked the song? Delete the old Eos cues in the group and run the script again.

---

## Why fire specific cues (not "go")

`/eos/cue/<n>/fire` jumps to an exact cue, so it doesn't matter where Eos currently sits — re‑runs
and out‑of‑order starts are safe. The alternative, `/eos/key/go_0` (press Go), just advances Eos's
next cue in sequence; one missed or double trigger and the song and lights drift apart. Firing by
number stays locked to the song.

If you *do* want plain Go steps instead, change the message in the generator to `/eos/key/go_0` and
drop the numbering.

---

## Verifying without touching the rig

You don't have to fire real Eos cues to test the timing. Temporarily set the trigger cues'
Destination to **TF3** (the bridge) instead of ETC and fire the group while tailing
`/tmp/tf3.out.log` on the Mac: each trigger shows up as `UNMATCHED OSC: /eos/cue/<n>/fire` at its
moment, proving the timeline fires the right messages at the right times. Set Destination back to
**ETC** for the real show.

See `QLAB-AUTOMATION.md` for the QLab‑scripting details and gotchas.
