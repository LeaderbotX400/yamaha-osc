#!/usr/bin/env python3
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

print()
if _failures:
    print(f"FAILED {len(_failures)}: {_failures}")
    sys.exit(1)
print("ALL SELF-TESTS PASSED")
