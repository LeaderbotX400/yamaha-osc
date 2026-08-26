-- generate_eos_triggers.applescript
--
-- Reads the SLICE MARKERS of a song (audio cue) and creates one Eos-trigger Network cue per slice,
-- with its pre-wait set to the slice time, inside the timeline group. Each cue FIRES a specific Eos
-- cue: /eos/cue/<n>/fire. Nothing is written to the Eos console.
--
-- USAGE
--   1. Build a TIMELINE GROUP in QLab with the song (an audio cue) as its FIRST cue.
--      (Group inspector -> Mode -> Timeline.)
--   2. Add slice markers to the song at each lighting trigger point (select the audio cue and press
--      the slice key, or use Cue -> Slices). The slice times are the trigger times.
--   3. Select the TIMELINE GROUP.
--   4. Run:  osascript generate_eos_triggers.applescript
--   5. REQUIRED: select the new Eos cues and set their Destination patch to ETC (192.168.50.175).
--      QLab 5.6.3 cannot set a cue's patch via AppleScript, so new cues land on the first patch;
--      retarget them to ETC once (Inspector -> Settings -> Destination).
--   6. Program Eos cues EOS_START, EOS_START+EOS_STEP, ... to hold the looks for each moment
--      (or change the numbering below to match your existing Eos cues).
--
-- Re-running after editing the slices: delete the old Eos cues in the group first, then run again.

property EOS_START : 1      -- first Eos cue number
property EOS_STEP : 1       -- increment between slices (use e.g. 1, or 0.5 for half-number cues)
property EOS_LIST : ""      -- "" = current Eos cue list; or a list number, e.g. "1"

on eosAddr(n)
  if EOS_LIST is "" then
    return "/eos/cue/" & n & "/fire"
  else
    return "/eos/cue/" & EOS_LIST & "/" & n & "/fire"
  end if
end eosAddr

tell application "QLab"
  tell front workspace
    set sel to (selected as list)
    if sel is {} then error "Select the Timeline Group (song as its first cue) before running."
    set grp to item 1 of sel
    if (q type of grp) is not "Group" then error "Select the Timeline GROUP itself, not a cue inside it."
    set kids to (cues of grp)
    if kids is {} then error "The group is empty. Put the song (audio cue) in it first."
    set songCue to item 1 of kids
    set marks to (slice markers of songCue)
    if marks is {} then error "The song cue has no slice markers. Add slices at your trigger points."

    set n to EOS_START
    set made to 0
    repeat with m in marks
      set t to (time of m)
      make type "Network"
      set c to last item of (selected as list)
      set custom message of c to my eosAddr(n)
      set q name of c to "Eos GO cue " & n & "  @ " & (t as string) & "s"
      set pre wait of c to (t as real)
      move c to end of grp
      set n to n + EOS_STEP
      set made to made + 1
    end repeat
    save
    return "Created " & made & " Eos-trigger cues in '" & (q name of grp) & ¬
      "'. NOW select them and set Destination = ETC."
  end tell
end tell
