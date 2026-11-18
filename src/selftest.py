#!/usr/bin/env python3
# Copyright (c) 2026 Eric Singer. All rights reserved.
# Proprietary and confidential; restricted distribution. No use without an explicit written grant - see LICENSE.
"""Offline unit tests for tf-bridge.py's pure logic (no console, no pythonosc needed).

Run anywhere:  python3 selftest.py
It stubs the pythonosc imports so tf-bridge.py can be imported without the package installed, then
exercises the dB<->raw conversion, the fade planner, curve shapes, selector parsing and fade-arg
parsing — the parts that are easy to get subtly wrong and that we could not all verify live.
"""

import importlib.util
import math
import os
import sys
import types

# --- stub pythonosc so importing tf-bridge does not require the package ------------------------
_disp = types.ModuleType("pythonosc.dispatcher")
_disp.Dispatcher = type("Dispatcher", (), {})
_srv = types.ModuleType("pythonosc.osc_server")
_srv.ThreadingOSCUDPServer = type("ThreadingOSCUDPServer", (), {})
_pkg = types.ModuleType("pythonosc")
sys.modules.setdefault("pythonosc", _pkg)
sys.modules.setdefault("pythonosc.dispatcher", _disp)
sys.modules.setdefault("pythonosc.osc_server", _srv)

_here = os.path.dirname(os.path.abspath(__file__))
_spec = importlib.util.spec_from_file_location("tf_bridge", os.path.join(_here, "tf-bridge.py"))
tb = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(tb)

_failures = []


def check(name, got, want):
    ok = got == want
    print(f"  [{'ok' if ok else 'FAIL'}] {name}: got {got!r}" + ("" if ok else f"  want {want!r}"))
    if not ok:
        _failures.append(name)


def approx(name, got, want, tol=1e-6):
    ok = abs(got - want) <= tol
    print(f"  [{'ok' if ok else 'FAIL'}] {name}: got {got!r}" + ("" if ok else f"  want ~{want!r}"))
    if not ok:
        _failures.append(name)


print("db_to_raw:")
check("0 dB", tb.db_to_raw(0.0), 0)
check("-6 dB", tb.db_to_raw(-6.0), -600)
check("-64 dB", tb.db_to_raw(-64.0), -6400)
check("+10 dB (max)", tb.db_to_raw(10.0), 1000)
check("+99 dB clamps to max", tb.db_to_raw(99.0), 1000)
check("-inf sentinel", tb.db_to_raw(float("-inf")), -32768)
check("rounds to nearest hundredth-dB (down)", tb.db_to_raw(-3.456), -346)
check("rounds to nearest hundredth-dB (up)", tb.db_to_raw(-3.454), -345)

print("parse_db_arg:")
check("'-inf'", tb.parse_db_arg("-inf"), float("-inf"))
check("'-6'", tb.parse_db_arg("-6"), -6.0)
check("int -6", tb.parse_db_arg(-6), -6.0)
check("garbage -> None", tb.parse_db_arg("loud"), None)

print("curve_shape:")
approx("linear(0.5)", tb.curve_shape("linear", 0.5), 0.5)
approx("cosine(0)", tb.curve_shape("cosine", 0.0), 0.0)
approx("cosine(1)", tb.curve_shape("cosine", 1.0), 1.0)
approx("cosine(0.5)", tb.curve_shape("cosine", 0.5), 0.5)
approx("exp(0.5)", tb.curve_shape("exp", 0.5), 0.25)

print("plan (fade steps):")
p = tb.plan(0, -4000, 1000, "linear")
check("linear ends on target", p[-1], -4000)
check("linear is monotonic down", all(b <= a for a, b in zip(p, p[1:])), True)
check("linear step count ~30", len(p), 30)
check("short fade is a jump", tb.plan(0, -4000, 10, "linear"), [-4000])
check("no-op fade", tb.plan(-1000, -1000, 5000, "linear"), [-1000])
pinf = tb.plan(0, -32768, 1000, "linear")
check("fade to -inf ends on sentinel", pinf[-1], -32768)
check("ramp stays on real scale before sentinel",
      all(tb.MIN_DB_RAW <= v <= 0 for v in pinf[:-1]), True)

print("parse_range_list / selector_to_indices:")
check("'1-4'", tb.parse_range_list("1-4", 1, 40), [1, 2, 3, 4])
check("'1,3,5'", tb.parse_range_list("1,3,5", 1, 40), [1, 3, 5])
check("'5'", tb.parse_range_list("5", 1, 40), [5])
check("out-of-range dropped", tb.parse_range_list("39-42", 1, 40), [39, 40])
check("selector 5 -> index 4", tb.selector_to_indices("InCh", "5"), [4])
check("selector 1-3 -> 0,1,2", tb.selector_to_indices("InCh", "1-3"), [0, 1, 2])

print("parse_fade_args:")
check("to+ms+curve", tb.parse_fade_args((-30, 1500, "cosine")), (-3000, 1500, "cosine", None))
check("to+ms defaults linear", tb.parse_fade_args((-40, 1000)), (-4000, 1000, "linear", None))
check("explicit from level", tb.parse_fade_args((-40, 1000, 0))[3], 0)
check("too few args -> None", tb.parse_fade_args((-40,)), None)


# --- fake console link: capture what the bridge would put on the wire ---------------------------
class FakeSock:
    def __init__(self):
        self.writes = []          # one entry per sendall (a batch)

    def sendall(self, data):
        self.writes.append(data.decode())

    def lines(self):
        return [ln for w in self.writes for ln in w.split("\r\n") if ln]


def with_fake(fn):
    """Run fn(fake) with get_socket/log stubbed and all bridge state reset."""
    fake = FakeSock()
    saved = (tb.get_socket, tb.log, tb.MIN_SEND_INTERVAL)
    tb.get_socket = lambda: fake
    tb.log = lambda m: None
    tb.MIN_SEND_INTERVAL = 0.0
    tb.active_fades.clear()
    tb.fader_shadow.clear()
    tb.desired_mute_state.clear()
    tb.applied_mute_state.clear()
    try:
        fn(fake)
    finally:
        tb.get_socket, tb.log, tb.MIN_SEND_INTERVAL = saved


print("batched writes:")
def t_batch(fake):
    tb.send_many([f"set X {i} 0 1" for i in range(20)])
    check("20 cmds, BATCH_MAX=8 -> 3 TCP writes", [w.count("\r\n") for w in fake.writes], [8, 8, 4])
    check("order preserved", fake.lines()[0::19], ["set X 0 0 1", "set X 19 0 1"])
with_fake(t_batch)

print("fade_value / rel_trim:")
f = tb.Fade("P", 0, 0, 0, -4000, 1000, "linear", t0=0.0)
check("start of fade", tb.fade_value(f, 0.0), (0, False))
check("midpoint linear", tb.fade_value(f, 0.5), (-2000, False))
check("end lands on target", tb.fade_value(f, 1.0), (-4000, True))
check("late tick still lands on target", tb.fade_value(f, 9.0), (-4000, True))
finf = tb.Fade("P", 0, 0, 0, tb.NEG_INF, 1000, "linear", t0=0.0)
check("ramp to -inf stays on real scale", tb.fade_value(finf, 0.5)[0] > tb.MIN_DB_RAW, True)
check("fade to -inf ends on sentinel", tb.fade_value(finf, 1.0), (tb.NEG_INF, True))
check("short fade is immediate", tb.fade_value(tb.Fade("P", 0, 0, 0, -400, 10, "linear", t0=0.0), 0.0), (-400, True))
check("rel +3", tb.rel_trim(-600, 3.0), -300)
check("rel clamps to +10 dB", tb.rel_trim(900, 5.0), 1000)
check("rel below floor -> -inf", tb.rel_trim(-13700, -5.0), tb.NEG_INF)
check("rel up from -inf", tb.rel_trim(tb.NEG_INF, 3.0), tb.MIN_DB_RAW + 300)
check("rel down from -inf stays", tb.rel_trim(tb.NEG_INF, -3.0), tb.NEG_INF)

print("one engine moves many fades, one batch per tick:")
def t_engine(fake):
    P = "MIXER:Current/InCh/Fader/Level"
    specs = [((P, i, 0), P, i, 0, -4000, 1000, "linear", 0) for i in range(24)]
    tb.start_fades_now(specs)
    t0 = 100.0
    for f in tb.active_fades.values():
        f.t0 = t0                # registered microseconds apart; pin for a deterministic test
    tb.fade_tick(now=t0 + 0.5)
    check("24 fades -> 24 writes in one tick", len(fake.lines()), 24)
    check("midpoint value", fake.lines()[0], f"set {P} 0 0 -2000")
    tb.fade_tick(now=t0 + 1.0)
    check("fades finish and are removed", len(tb.active_fades), 0)
    check("shadow ends on target", tb.fader_shadow[(P, 7, 0)], -4000)
with_fake(t_engine)

print("direct write supersedes a fade (last writer wins):")
def t_cancel(fake):
    P = "MIXER:Current/InCh/Fader/Level"
    tb.start_fades_now([((P, 4, 0), P, 4, 0, -4000, 1000, "linear", 0)])
    t0 = tb.active_fades[(P, 4, 0)].t0
    tb.fade_tick(now=t0 + 0.2)
    tb.set_level_direct(P, 4, 0, -600)
    n = len(fake.lines())
    tb.fade_tick(now=t0 + 0.4)
    check("no fade write after the direct write", len(fake.lines()), n)
    check("direct value is the last write", fake.lines()[-1], f"set {P} 4 0 -600")
    check("fade is gone", len(tb.active_fades), 0)
with_fake(t_cancel)

print("grammar -> commands:")
def t_groups(fake):
    tb.on_osc("/ch/1-5|12-24/on", "off", "on")
    d = tb.desired_mute_state
    check("1-5 off", all(d[("input", n)] == 0 for n in range(1, 6)), True)
    check("12-24 on", all(d[("input", n)] == 1 for n in range(12, 25)), True)
    check("6-11 untouched", any(("input", n) in d for n in range(6, 12)), False)
    check("stateful writes wait for the sync thread", fake.lines(), [])
    tb.synchronize_states()
    check("sync sends all 18 as one burst", len(fake.lines()), 18)
with_fake(t_groups)

def t_groups_extra(fake):
    tb.on_osc("/ch/1-5|12-24/on", "off", "on", "off")      # 3 values, 2 groups: extra ignored
    check("extra value ignored", tb.desired_mute_state[("input", 12)], 1)
    tb.desired_mute_state.clear()
    tb.on_osc("/ch/1-3|7/mute", 1)                          # one value applies to all groups
    check("single value -> every group", sorted(k[1] for k, v in tb.desired_mute_state.items() if v == 0), [1, 2, 3, 7])
with_fake(t_groups_extra)

def t_fader_groups(fake):
    tb.on_osc("/ch/1-2|5/fader", -6, "-inf")
    check("fader per group", fake.lines(), [
        "set MIXER:Current/InCh/Fader/Level 0 0 -600",
        "set MIXER:Current/InCh/Fader/Level 1 0 -600",
        "set MIXER:Current/InCh/Fader/Level 4 0 -32768"])
    check("one TCP write for the message", len(fake.writes), 1)
with_fake(t_fader_groups)

def t_only(fake):
    tb.on_osc("/ch/1-24/only", "3,7,12")
    d = tb.desired_mute_state
    check("listed channels on", sorted(k[1] for k, v in d.items() if v == 1), [3, 7, 12])
    check("rest of 1-24 off", len([1 for v in d.values() if v == 0]), 21)
    check("nothing outside scope touched", max(k[1] for k in d), 24)
    tb.desired_mute_state.clear()
    tb.on_osc("/ch/1-8/only", 2, 4.0)                        # numeric args (QLab sends numbers)
    check("numeric args", sorted(k[1] for k, v in tb.desired_mute_state.items() if v == 1), [2, 4])
    tb.desired_mute_state.clear()
    tb.on_osc("/ch/1-8/only", "30")                          # outside scope: ignored, all off
    check("out-of-scope keep ignored", sorted(set(tb.desired_mute_state.values())), [0])
    tb.desired_mute_state.clear()
    tb.on_osc("/ch/1|3/only", "1")
    check("'|' rejected for only", tb.desired_mute_state, {})
with_fake(t_only)

def t_direct_strips(fake):
    tb.on_osc("/st/1/mute", 1)
    check("stereo master stays direct", fake.lines(), ["set MIXER:Current/St/Fader/On 0 0 0"])
    tb.on_osc("/dca/1/mute", 1)
    tb.synchronize_states()
    check("dca stateful, dual-written", fake.lines()[1:], [
        "set MIXER:Current/DCA/Fader/On 0 0 0", "set MIXER:Current/DcaCh/Fader/On 0 0 0"])
with_fake(t_direct_strips)

def t_scene_reassert(fake):
    tb.on_osc("/ch/1-3/mute", 1)
    tb.synchronize_states()
    tb.applied_mute_state.clear()                            # what a scene recall does
    fake.writes.clear()
    tb.synchronize_states()
    check("mutes re-asserted as one burst after a scene", [w.count("\r\n") for w in fake.writes], [3])
with_fake(t_scene_reassert)

def t_rel(fake):
    P = "MIXER:Current/InCh/Fader/Level"
    tb.fader_shadow[(P, 0, 0)] = -600
    tb.on_osc("/ch/1/fader/rel", -3)
    check("relative trim from shadow", fake.lines(), [f"set {P} 0 0 -900"])
with_fake(t_rel)

def t_stop(fake):
    P = "MIXER:Current/InCh/Fader/Level"
    tb.start_fades_now([((P, i, 0), P, i, 0, -4000, 5000, "linear", 0) for i in range(4)])
    tb.on_osc("/ch/1-2/fade/stop")
    check("fade/stop only those", sorted(k[1] for k in tb.active_fades), [2, 3])
    tb.on_osc("/panic")
    check("/panic stops all", len(tb.active_fades), 0)
with_fake(t_stop)

print("presets:")
check("parse preset line", tb.parse_osc_line('/ch/5/name "Lead Vox"'), ("/ch/5/name", ["Lead Vox"]))
check("numbers coerced", tb.parse_osc_line("/ch/5/fade -30 1500 cosine"), ("/ch/5/fade", [-30, 1500, "cosine"]))
check("lists stay strings", tb.parse_osc_line("/ch/1-24/only 3,7,12"), ("/ch/1-24/only", ["3,7,12"]))
def t_preset(fake):
    import json, tempfile
    with tempfile.TemporaryDirectory() as d:
        tb.PRESETS_PATH = os.path.join(d, "presets.json")
        with open(tb.PRESETS_PATH, "w") as fh:
            json.dump({"sc2": ["/ch/1-2/fader -6", "/ch/3/fader -10", "/st/1/mute 1"]}, fh)
        tb.presets_mtime = None
        tb.on_osc("/preset/sc2")
    check("preset's direct writes are one batch", len(fake.writes), 1)
    check("preset line count", len(fake.lines()), 4)
    check("collect context cleared", getattr(tb._tls, "collect", None), None)
with_fake(t_preset)

print("console replies:")
def t_replies(fake):
    logged = []
    tb.log = logged.append
    tb._handle_console_line('ERROR set MIXER:Current/InCh/Fader/Level 99 0 InvalidIndex')
    check("ERROR line is logged", [m for m in logged if "CONSOLE ERROR" in m] != [], True)
    key = ("MIXER:Current/InCh/Fader/Level", 4, 0)
    tb._handle_console_line("NOTIFY set MIXER:Current/InCh/Fader/Level 4 0 -1234 \"-12.34\"")
    check("desk move updates shadow", tb.fader_shadow.get(key), -1234)
    w = [tb.threading.Event(), None]
    tb.pending_gets[key] = [w]
    tb._handle_console_line("OK get MIXER:Current/InCh/Fader/Level 4 0 -700")
    check("get reply resolves its waiter", (w[0].is_set(), w[1]), (True, -700))
    tb.start_fades_now([(key, key[0], 4, 0, -4000, 1000, "linear", 0)])
    tb._handle_console_line("NOTIFY set MIXER:Current/InCh/Fader/Level 4 0 -5 \"x\"")
    check("shadow not clobbered while our fade owns it", tb.fader_shadow.get(key), -700)
with_fake(t_replies)

print("virtual DCAs:")
def vd_reset():
    tb.vdca_members.clear(); tb.vdca_offset.clear(); tb.vdca_base.clear()
def lv(ch): return tb.fader_shadow.get((tb.INCH_LEVEL, ch - 1, 0))
check("vdca_level -5 dB", tb.vdca_level(-600, -5.0), -1100)
check("vdca_level clamps at +10 dB", tb.vdca_level(900, 5.0), 1000)
check("vdca_level -inf member stays -inf", tb.vdca_level(tb.NEG_INF, 3.0), tb.NEG_INF)
check("vdca_level -inf offset", tb.vdca_level(-600, float("-inf")), tb.NEG_INF)
check("vdca_level below floor -> -inf", tb.vdca_level(-13700, -5.0), tb.NEG_INF)

def t_vdca(fake):
    vd_reset()
    for c, v in ((2, -600), (3, -400), (4, 0), (6, -1000)):
        tb.fader_shadow[(tb.INCH_LEVEL, c - 1, 0)] = v
    tb.on_osc("/vdca/2/only", "3,4,6"); 
    check("members set", sorted(tb.vdca_members[2]), [3, 4, 6])
    check("joining alone changes nothing (offset 0)", [lv(3), lv(4), lv(6)], [-400, 0, -1000])
    tb.on_osc("/vdca/2/fader", -5)
    check("group -5 dB applied to members", [lv(3), lv(4), lv(6)], [-900, -500, -1500])
    check("non-member untouched", lv(2), -600)
    tb.on_osc("/vdca/2/only", "4")
    check("removed members restored to base", [lv(3), lv(6)], [-400, -1000])
    check("remaining member keeps offset", lv(4), -500)
    tb.on_osc("/vdca/1/only", "4")
    tb.on_osc("/vdca/1/fader", 2)
    check("two groups add (-5 + 2)", lv(4), -300)
    tb.on_osc("/vdca/2/clear")
    check("leaving one group keeps the other's offset", lv(4), 200)
    tb.on_osc("/vdca/1/clear")
    check("leaving the last group restores base", lv(4), 0)
    check("base released", tb.vdca_base, {})
with_fake(t_vdca)

def t_vdca_direct(fake):
    vd_reset()
    tb.fader_shadow[(tb.INCH_LEVEL, 2, 0)] = -400
    tb.on_osc("/vdca/1/only", "3"); tb.on_osc("/vdca/1/fader", -5)
    tb.on_osc("/ch/3/fader", -10)                    # user moves the fader directly
    check("direct write lands as written", lv(3), -1000)
    tb.on_osc("/vdca/1/fader", 0)
    check("then group 0 dB returns to the new base", lv(3), -500)
with_fake(t_vdca_direct)

def t_vdca_fade(fake):
    vd_reset()
    tb.fader_shadow[(tb.INCH_LEVEL, 4, 0)] = 0
    tb.on_osc("/vdca/3/only", "5")
    tb.on_osc("/vdca/3/fade", -5, 1000)
    f = tb.active_fades.get((tb.INCH_LEVEL, 4, 0))
    check("fade registered to base+offset", (f.from_raw, f.to_raw, f.dur) if f else None, (0, -500, 1000))
    tb.on_osc("/vdca/9/fader", -5)
    check("vdca 9 rejected", 9 in tb.vdca_members, False)
with_fake(t_vdca_fade)

print("send only / prepost:")
def t_send_only(fake):
    tb.on_osc("/ch/1-6/send/mix/2/only", "2,5")
    got = {(int(x.split()[2]), x.split()[3]): x.split()[4] for x in fake.lines()}
    check("only: listed on, rest of scope off", sorted((i + 1, v) for (i, d), v in got.items()),
          [(1, "0"), (2, "1"), (3, "0"), (4, "0"), (5, "1"), (6, "0")])
    check("dest 2 -> index 1", {d for (i, d) in got}, {"1"})
    fake.writes.clear()
    tb.on_osc("/ch/1-3/send/mix/2-3/only")
    check("no args = all off, both dests", sorted(x.split()[4] for x in fake.lines()), ["0"] * 6)
    fake.writes.clear()
    tb.on_osc("/ch/1-3/send/mix/3/only", "9")
    check("out-of-scope keep ignored", sorted(x.split()[4] for x in fake.lines()), ["0"] * 3)
    fake.writes.clear()
    tb.on_osc("/ch/1-2/send/mix/3/prepost", 0)
    check("prepost raw", fake.lines(), ["set MIXER:Current/InCh/ToMix/PrePost 0 2 0", "set MIXER:Current/InCh/ToMix/PrePost 1 2 0"])
    fake.writes.clear()
    tb.on_osc("/ch/1|2/send/mix/3/only", "1")
    check("'|' rejected for send only", fake.lines(), [])
with_fake(t_send_only)

print("scene recall vs remembered state:")
def t_scene_wins(fake):
    tb.on_osc("/ch/1-3/on", 0)                      # QLab-style cue: remembered desired state
    tb.synchronize_states()
    check("states remembered", len(tb.desired_mute_state), 3)
    sent, real = [], tb.set_desired_scene
    tb.set_desired_scene = lambda b, n: sent.append((b, n))
    tb.on_osc("/scene/recall", "a", 76)
    check("recall forgets remembered state (scene wins)", dict(tb.desired_mute_state), {})
    check("scene still requested", sent, [("A", 76)])
    tb.on_osc("/ch/1-3/on", 0)
    tb.on_osc("/scene/recall", "a", 77, "keep")
    check("`keep` retains remembered state", len(tb.desired_mute_state), 3)
    tb.set_desired_scene = real
with_fake(t_scene_wins)

print()
if _failures:
    print(f"FAILED {len(_failures)}: {_failures}")
    sys.exit(1)
print("ALL SELF-TESTS PASSED")
