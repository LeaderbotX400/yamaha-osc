-- generate.applescript — build the "TF BRIDGE TEMPLATES" cue group in the front QLab workspace.
--
-- Creates one Network (OSC) cue per bridge function, named/numbered, addressed with the new
-- /ch|mix|dca|... grammar. Run against QLab 5 (tested on 5.6.3) with the target workspace frontmost:
--
--     osascript generate.applescript
--
-- NOTES / QLab 5.6.3 quirks learned the hard way:
--  * The Network-cue type string is "Network" (QLab 3's "OSC" is rejected).
--  * make/duplicate do NOT return a usable reference — grab the new cue via
--    `last item of (selected as list)` right after `make`.
--  * `duplicate <cue> to <loc>` is unsupported (errors -10006); `move <cue> to end of <group>` works.
--  * A cue's destination `patch` is NOT settable via AppleScript in 5.6.3 (integer/name/inherit all
--    fail). So new cues get the workspace's DEFAULT patch (patch #1). Make the bridge patch (pointing
--    at 127.0.0.1:8000, UDP) the FIRST network patch in the workspace and every new cue lands on it.
--    Otherwise: select the whole group afterward and set Destination in the inspector once.
--
-- The bridge listens on UDP 8000 on the same Mac, so the patch destination is 127.0.0.1:8000.

tell application "QLab"
  tell front workspace
    make type "Group"
    set grp to last item of (selected as list)
    set q name of grp to "TF BRIDGE TEMPLATES - set Destination = TF3"
    set q number of grp to "TFB"
    set cueData to {¬
      {"501", "CH 1 - fader to dB", "/ch/1/fader 0.0"}, ¬
      {"502", "CH 1 - fader to -inf", "/ch/1/fader -inf"}, ¬
      {"503", "CH 1 - fade to -30 over 2s", "/ch/1/fade -30.0 2000 cosine"}, ¬
      {"504", "CH 1 - fade up to 0 over 1.5s", "/ch/1/fade 0.0 1500 cosine"}, ¬
      {"505", "CH 1 - MUTE", "/ch/1/mute 1"}, ¬
      {"506", "CH 1 - UNMUTE", "/ch/1/mute 0"}, ¬
      {"507", "CH 1 - ON", "/ch/1/on 1"}, ¬
      {"508", "CH 1 - name", "/ch/1/name \"Lead Vox\""}, ¬
      {"509", "CH 1 - colour Blue", "/ch/1/color Blue"}, ¬
      {"510", "CH 1-8 - MUTE (range)", "/ch/1-8/mute 1"}, ¬
      {"521", "CH 1 to Mix 3 - send level", "/ch/1/send/mix/3/fader -6.0"}, ¬
      {"522", "CH 1 to Mix 3 - send on", "/ch/1/send/mix/3/on 1"}, ¬
      {"523", "CH 1 to Mix 3 - send fade", "/ch/1/send/mix/3/fade -20.0 1500"}, ¬
      {"541", "MIX 3 - fader", "/mix/3/fader 0.0"}, ¬
      {"542", "MATRIX 1 - fader", "/mtrx/1/fader 0.0"}, ¬
      {"543", "STEREO - fader", "/st/1/fader 0.0"}, ¬
      {"544", "MONO - fader", "/mono/1/fader 0.0"}, ¬
      {"561", "DCA 1 - fader", "/dca/1/fader 0.0"}, ¬
      {"562", "DCA 1 - MUTE", "/dca/1/mute 1"}, ¬
      {"581", "MUTE GROUP 1 - engage", "/mutegroup/1 1"}, ¬
      {"582", "MUTE GROUP 1 - release", "/mutegroup/1 0"}, ¬
      {"591", "SCENE - recall A12", "/scene/recall a 12"}, ¬
      {"592", "SCENE - recall B03", "/scene/recall b 3"}}
    set madeCount to 0
    repeat with row in cueData
      make type "Network"
      set c to last item of (selected as list)
      set q number of c to (item 1 of row)
      set q name of c to (item 2 of row)
      set custom message of c to (item 3 of row)
      move c to end of grp
      set madeCount to madeCount + 1
    end repeat
    save
    return "created group + " & madeCount & " cues; saved"
  end tell
end tell
