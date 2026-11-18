#!/usr/bin/env python3
# Copyright (c) 2026 Eric Singer. All rights reserved.
# Proprietary and confidential; restricted distribution. No use without an explicit written grant - see LICENSE.
"""Build the `MIC <n> - <SONG> (A<slot>)` cues for "Christmas Tree Confessions" (from "CTC - Song List Tech.pdf").

    python3 build_mic_cues.py --check           # validate the data against the bridge's own parser
    python3 build_mic_cues.py                   # (re)write generate_mic_cues.applescript next to this file
    python3 build_mic_cues.py --migrate-script  # print AppleScript turning the LIVE per-song groups into flat cues

Scene-only design: the console scenes (A76..A97, one per number, stored on the desk) carry everything - mic on/off and the DCA
layout - so each number is ONE flat Network cue, `/scene/recall a <slot>` with MIC n -> A(75+n). The notes of each cue hold the
mic set and the leads/backing DCA split from the tech document for reference (leads = Solo list + speaking-only channels not in
the Chorus list; backing = the other active mics). Nothing else is sent, so any mic on/off state the bridge remembers from
earlier QLab cues is re-asserted over a recalled scene (see PENDING-LIVE.md).
"""
import importlib.util
import os
import sys
import types

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, "generate_mic_cues.applescript")

# (n, song, active, leads, backing, note) -- channel selectors in the bridge's own range/list syntax.
SONGS = [
    (1, "CHRISTMAS CHEER", "2-24", "2-5,14", "6-13,15-24",
     "3 Poppy/4 Francis/5 Holly never sing over anyone: chorus-level mic'ing works. 14 Hattie carries; if raised, bring back to chorus level after her solo. Brassheart sings over the chorus only at the end."),
    (2, "MOODY WEISENBROOD", "2-24", "6-10", "2-5,11-24",
     "No solo sings over the chorus, but all chorus mics down a touch (a lot of mics at once)."),
    (3, "CHRISTMAS CHEER REPRISE", "2", "2", "", "Solo: Brassheart."),
    (4, "CAROL ONE/TWO/THREE", "19-21", "19-21", "", "Solos: 19 Patty (Alto), 20 Brenda (2 Sop), 21 Veronica (1 Sop)."),
    (5, "THE STORY", "1,22,23", "1,22,23", "", "1 Traci lead; 22 Fred and 23 Elodie back up."),
    (6, "LAST YEAR ON THE TREE - PART A and B", "1-5,24", "1-5,24", "", "Solos 2-5; spoken lines 1 Traci, 24 Agnes."),
    (7, "THE ABDUCTION", "1-18,22,23", "1-5", "6-18,22,23",
     "Brassheart (2) has a big solo at the end and must be louder than the chorus - the only moment the chorus sings with a soloist. 2-5 join the chorus when not soloing. Female chorus 3,5,6-10,12,13,14,16,23; male 2,4,11,15,17,18,22."),
    (8, "ANGELS", "1,19-21", "1,19-21", "", "Solos 19 Patty (lead), 20 Brenda (alto), 21 Veronica (sop); speaking 1 Traci."),
    (9, "IT'S BEEN A BAD YEAR", "2-18,22-24", "24", "2-18,22,23", "Solo 24 Agnes; chorus 2-18, 22, 23."),
    (10, "CRISPIN PART A", "2,4,5", "2,4,5", "", "Solo 2 Brassheart; speaking 4 Francis, 5 Holly."),
    (11, "THE DEAL", "1-10", "1,2,6-10", "3-5",
     "Solos 2, 6 Heart Attack (main lead), 7-10; chorus 3-5; speaking 1, 2, 6."),
    (12, "IMPERFECT FRED", "22", "22", "", "Solo 22 Fred."),
    (13, "IMPERFECT COMMITTEE", "1-13,22", "11-13,22", "1-10", "Solos 22 Fred, 11 Mr Darcy, 12 Trixie, 13 Sugarplum; chorus 1-10."),
    (14, "CRISPIN PART B", "2,5", "2,5", "", "Solo 2 Brassheart; speaking 2, 5 Holly."),
    (15, "MOODY WEISENBROOD REPRISE", "1,2,15", "1,2,15", "", "Solos 1 Traci, 2 Brassheart, 15 Crispin."),
    (16, "STAR TOPPER", "1,3-13,15,16", "3,15,16", "1,4-13",
     "Solos 15 Barny Bow, 16 Barby Bow, 3 Poppy; chorus 1,3-13; speaking 11, 1, 3."),
    (17, "CHRISTMAS DON'T HAVE TIME", "2,3", "2,3", "", "Solos 2 Brassheart, 3 Poppy."),
    (18, "THE CLIMB", "1-13,15,16,18-21", "1,2", "3-13,15,16,18-21",
     "Brassheart and Traci sing in unison over the chorus at the end and must be heard."),
    (19, "BETTER DAYS", "11-13,23", "11-13,23", "", "Solos 23 Elodie (main lead), 11 Mr Darcy, 12 Trixie, 13 Sugarplum."),
    (20, "I AM THE STAR", "1-13,15,16,18,19,21", "18", "1-13,15,16,19,21",
     "Solo 18 Star Topper. Sop/alto split at the end may need adjusting: sops 13, 9, 6; altos 12, 15."),
    (21, "WHY CAN'T I DO THIS?", "1-16", "1-5", "6-16",
     "Brassheart (2) is main soloist throughout, with a solo over the chorus; the others have one line each. Speaking 1, 2, 3, 4, 14 Hattie."),
    (22, "IT'S A NEW DAY", "1-24", "1,2", "3-24",
     "Traci and Brassheart start as soloists; as the chorus enters they sing in groups, all at chorus level. Groups: 1-5, 6-10, 11-13, 22-24, others 14-21."),
]


def load_bridge():
    for name, attrs in (("pythonosc", {}), ("pythonosc.dispatcher", {"Dispatcher": type("D", (), {})}),
                        ("pythonosc.osc_server", {"ThreadingOSCUDPServer": type("T", (), {})})):
        m = types.ModuleType(name)
        m.__dict__.update(attrs)
        sys.modules.setdefault(name, m)
    spec = importlib.util.spec_from_file_location("tb", os.path.join(HERE, "..", "tf-bridge.py"))
    tb = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(tb)
    return tb


SCENE_BASE = 76         # MIC n -> scene A(SCENE_BASE + n - 1): MIC 1 = A76 ... MIC 22 = A97


def scene_slot(n):
    return SCENE_BASE + n - 1


def cue_name(n, song):
    return f"MIC {n} - {song} (A{scene_slot(n)})"


def cue_msg(n):
    return f"/scene/recall a {scene_slot(n)}"


def cue_notes(n, active, leads, backing, note):
    return f"Mics on {active} | leads {leads} | backing {backing or '-'} | {note}"


def check():
    tb = load_bridge()
    bad = []
    ns = [x[0] for x in SONGS]
    if ns != list(range(1, len(SONGS) + 1)):
        bad.append(f"numbers not 1..N: {ns}")
    for n, song, active, leads, backing, _ in SONGS:
        a, l, b = (set(tb.parse_range_list(x, 1, 24)) for x in (active, leads, backing))
        for label, sel in (("active", active), ("leads", leads), ("backing", backing)):
            if sel and not tb.parse_range_list(sel, 1, 24):
                bad.append(f"MIC {n} {song}: {label} {sel!r} parses empty")
        if l | b != a:
            bad.append(f"MIC {n} {song}: leads+backing != active (missing {sorted(a - l - b)}, extra {sorted((l | b) - a)})")
        if l & b:
            bad.append(f"MIC {n} {song}: leads and backing overlap {sorted(l & b)}")
        if not l:
            bad.append(f"MIC {n} {song}: no leads")
        if not 0 <= scene_slot(n) <= 99:
            bad.append(f"MIC {n}: scene slot {scene_slot(n)} out of range")
    for b in bad:
        print("FAIL", b)
    print(f"{len(SONGS)} numbers -> scenes A{scene_slot(1)}..A{scene_slot(len(SONGS))}:", "OK" if not bad else f"{len(bad)} problem(s)")
    return not bad


def aq(s):
    return s.replace("\\", "\\\\").replace('"', '\\"')


def rows():
    return ", ¬\n".join(
        f'    {{{n}, "{aq(cue_name(n, song))}", "{aq(cue_msg(n))}", "{aq(cue_notes(n, active, leads, backing, note))}"}}'
        for n, song, active, leads, backing, note in SONGS)


MAKE_CUES = '''    repeat with row in cueRows
      set nm to item 2 of row
      set have to false
      repeat with k in (cues of micTop)
        if (q name of k) is nm then set have to true
      end repeat
      if not have then
        make type "Network"
        set c to last item of (selected as list)
        set q name of c to nm
        set custom message of c to (item 3 of row)
        try
          set notes of c to (item 4 of row)
        end try
        move c to end of micTop
        set madeCount to madeCount + 1
      end if
    end repeat
'''


def build():
    return f'''-- generate_mic_cues.applescript  (GENERATED by build_mic_cues.py - edit the data there, not here)
-- Copyright (c) 2026 Eric Singer. All rights reserved. Proprietary; see LICENSE.
--
-- Adds ONE top-level group "MIC CUES" at the end of the front QLab workspace holding {len(SONGS)} flat Network cues,
-- "MIC <n> - <SONG> (A<slot>)", each sending /scene/recall a <slot>. Existing cues are not touched.
-- BEFORE RUNNING: Edit mode (not Show mode), a saved copy of the show, and afterwards set the new cues' patch to TF3
-- (a script cannot set a cue's patch). Run on the booth Mac:  osascript generate_mic_cues.applescript

tell application "QLab"
  tell front workspace
    make type "Group"
    set micTop to last item of (selected as list)
    set q name of micTop to "MIC CUES"
    set madeCount to 0
    set cueRows to {{¬
{rows()}}}
{MAKE_CUES}    save
    return "created " & madeCount & " flat MIC cues inside MIC CUES; saved"
  end tell
end tell
'''


def migrate_script():
    """AppleScript for the LIVE show: add the flat scene cues to MIC CUES, then delete the old per-song groups."""
    return f'''tell application "QLab"
  tell front workspace
    set micTop to item 1 of (every cue whose q name is "MIC CUES")
    set madeCount to 0
    set cueRows to {{¬
{rows()}}}
{MAKE_CUES}    set removedCount to 0
    repeat with i from (count of cues of micTop) to 1 by -1
      set k to item i of (cues of micTop)
      if (q type of k) is "Group" and (q name of k) starts with "MIC " then
        delete k
        set removedCount to removedCount + 1
      end if
    end repeat
    save
    return "added " & madeCount & " flat cues, removed " & removedCount & " old groups; saved"
  end tell
end tell
'''


if __name__ == "__main__":
    if "--migrate-script" in sys.argv:
        print(migrate_script())
        sys.exit(0)
    if "--check" in sys.argv:
        sys.exit(0 if check() else 1)
    if not check():
        sys.exit(1)
    open(OUT, "w", encoding="utf-8").write(build())
    print("wrote", OUT)
